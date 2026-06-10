//! Forgejo integration via the API (using curl).
//!
//! Forgejo is a self-hosted forge (fork of Gitea). This implementation uses
//! the REST API via curl for operations. It also supports Gitea instances.

use crate::forge::models::*;
use crate::forge::Forge;
use crate::remote::GitLoc;
use serde::Deserialize;

pub struct ForgejoForge;

impl ForgejoForge {
    /// Run `curl` against the Forgejo API with the given endpoint and method.
    fn api_request(
        &self,
        loc: &GitLoc,
        endpoint: &str,
        method: &str,
    ) -> Result<String, ForgeError> {
        // Get the API base URL from git config or construct from remote
        let remote_url = loc
            .git_out(&["config", "--get", "remote.origin.url"])
            .unwrap_or_default();
        let api_base = self.get_api_base(&remote_url)?;
        let url = format!("{}{}", api_base, endpoint);

        // Get token from environment or git config
        let token = self.get_token(loc)?;

        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                method,
                "-H",
                &format!("Authorization: token {}", token),
                "-H",
                "Accept: application/json",
                &url,
            ])
            .output()
            .map_err(|e| ForgeError::Other(e.to_string()))?;

        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).to_lowercase();
            Err(classify_forgejo_error(&stderr))
        }
    }

    /// Get the API base URL from the remote URL.
    fn get_api_base(&self, remote_url: &str) -> Result<String, ForgeError> {
        // Handle SSH URLs like git@codeberg.org:user/repo.git
        // Handle HTTPS URLs like https://codeberg.org/user/repo.git
        let host = if remote_url.contains("@") {
            // SSH format: git@host:path
            remote_url
                .split('@')
                .nth(1)
                .and_then(|s| s.split(':').next())
                .or_else(|| {
                    remote_url
                        .split('@')
                        .nth(1)
                        .map(|s| s.split('/').next().unwrap_or(s))
                })
        } else {
            // HTTPS format: https://host/path
            remote_url
                .split("://")
                .nth(1)
                .map(|s| s.split('/').next().unwrap_or(s))
        };

        let host =
            host.ok_or_else(|| ForgeError::Other("Could not parse host from remote URL".into()))?;

        // Default ports for common forges
        let api_base = match host {
            "github.com" => format!("https://api.github.com"),
            h if h.contains("codeberg.org") => format!("https://codeberg.org/api/v1"),
            h if h.contains("forgejo") || h.contains("gitea") => {
                // Try to detect if it's on a custom port
                if host.contains(":") {
                    let (hostname, port) = h.split_once(':').unwrap_or((h, ""));
                    format!("http://{}:{}/api/v1", hostname, port)
                } else {
                    format!("https://{}/api/v1", host)
                }
            }
            _ => format!("https://{}/api/v1", host),
        };

        Ok(api_base)
    }

    /// Get the API token from environment or git config.
    fn get_token(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        // First try environment variable
        if let Ok(token) = std::env::var("FORGEJO_TOKEN") {
            return Ok(token);
        }
        if let Ok(token) = std::env::var("GITEA_TOKEN") {
            return Ok(token);
        }
        if let Ok(token) = std::env::var("GITHUB_TOKEN") {
            return Ok(token);
        }

        // Try git config
        if let Some(token) = loc.git_out(&["config", "--get", "forgejo.token"]) {
            if !token.is_empty() {
                return Ok(token);
            }
        }
        if let Some(token) = loc.git_out(&["config", "--get", "gitea.token"]) {
            if !token.is_empty() {
                return Ok(token);
            }
        }

        Err(ForgeError::NotAuthenticated)
    }

    /// Get the owner and repo from the remote URL.
    fn get_owner_repo(&self, remote_url: &str) -> Result<(String, String), ForgeError> {
        let path = if let Some(rest) = remote_url
            .split_once(':')
            .map(|(_, r)| r)
            .filter(|_| remote_url.contains('@') && !remote_url.contains("://"))
        {
            rest.to_string()
        } else if let Some(idx) = remote_url.find("://") {
            let after = &remote_url[idx + 3..];
            after.split_once('/').map(|(_, r)| r.to_string()).ok_or_else(|| ForgeError::Other("Could not parse path from remote URL".into()))?
        } else {
            return Err(ForgeError::Other("Could not parse path from remote URL".into()));
        };

        let path = path.trim_end_matches(".git");
        let mut parts = path.split('/');
        let owner = parts.next().ok_or_else(|| ForgeError::Other("Could not parse owner/repo from remote URL".into()))?;
        let repo = parts.next().ok_or_else(|| ForgeError::Other("Could not parse owner/repo from remote URL".into()))?;

        Ok((owner.to_string(), repo.to_string()))
    }
    /// Get the current branch name.
    fn get_branch(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        loc.git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .ok_or_else(|| ForgeError::Other("Could not resolve branch".into()))
    }
}

/// Classify Forgejo API errors.
fn classify_forgejo_error(stderr: &str) -> ForgeError {
    if stderr.contains("command not found")
        || stderr.contains("not found")
        || stderr.contains("curl:")
    {
        ForgeError::NotInstalled
    } else if stderr.contains("authentication")
        || stderr.contains("401")
        || stderr.contains("unauthorized")
        || stderr.contains("token")
    {
        ForgeError::NotAuthenticated
    } else if stderr.contains("rate limit") || stderr.contains("429") {
        ForgeError::RateLimited
    } else if stderr.contains("no pull request")
        || stderr.contains("not found")
        || stderr.contains("404")
    {
        ForgeError::NoPr
    } else {
        ForgeError::Other(stderr.trim().to_string())
    }
}

impl Forge for ForgejoForge {
    fn pr_status(&self, loc: &GitLoc) -> PrPanel {
        let branch = self.get_branch(loc).unwrap_or_default();
        let remote_url = match loc.git_out(&["config", "--get", "remote.origin.url"]) {
            Some(url) => url,
            None => {
                return PrPanel {
                    state: PanelState::Error {
                        message: "No remote origin".into(),
                    },
                    worktree: loc.path(),
                    branch,
                    fetched_at: crate::util::now(),
                };
            }
        };

        let (owner, repo) = match self.get_owner_repo(&remote_url) {
            Ok(pair) => pair,
            Err(e) => {
                return PrPanel {
                    state: PanelState::Error {
                        message: e.message(),
                    },
                    worktree: loc.path(),
                    branch,
                    fetched_at: crate::util::now(),
                };
            }
        };

        // Try to find PR for this branch
        let endpoint = format!("/repos/{}/{}/pulls?state=open&head={}", owner, repo, branch);

        let state = match self.api_request(loc, &endpoint, "GET") {
            Ok(json) => {
                // Parse the response to find a PR for this branch
                match serde_json::from_str::<Vec<ForgejoPullRequest>>(&json) {
                    Ok(prs) => {
                        if let Some(pr) = prs.into_iter().find(|p| p.head.ref_name == branch) {
                            // Now get detailed info with checks/status
                            let detail_endpoint =
                                format!("/repos/{}/{}/pulls/{}", owner, repo, pr.number);
                            if let Ok(detail_json) = self.api_request(loc, &detail_endpoint, "GET")
                            {
                                if let Ok(mut pr_detail) =
                                    serde_json::from_str::<ForgejoPullRequestDetail>(&detail_json)
                                {
                                    // Get combined status
                                    let status_endpoint = format!(
                                        "/repos/{}/{}/commits/{}/status",
                                        owner, repo, branch
                                    );
                                    let checks = if let Ok(status_json) =
                                        self.api_request(loc, &status_endpoint, "GET")
                                    {
                                        if let Ok(status) =
                                            serde_json::from_str::<ForgejoCombinedStatus>(
                                                &status_json,
                                            )
                                        {
                                            summarize(&status.statuses)
                                        } else {
                                            ChecksSummary::default()
                                        }
                                    } else {
                                        ChecksSummary::default()
                                    };

                                    PanelState::Pr(Box::new(PrStatus {
                                        number: pr_detail.number,
                                        title: pr_detail.title,
                                        state: pr_detail.state,
                                        url: pr_detail.html_url,
                                        is_draft: pr_detail.draft,
                                        head_ref_name: pr_detail.head.ref_name,
                                        base_ref_name: pr_detail.base.ref_name,
                                        mergeable: "".into(),
                                        merge_state_status: "".into(),
                                        review_decision: pr_detail
                                            .mergeable
                                            .then(|| "APPROVED".to_string()),
                                        status_check_rollup: vec![],
                                        checks,
                                    }))
                                } else {
                                    PanelState::Pr(Box::new(PrStatus {
                                        number: pr.number,
                                        title: pr.title,
                                        state: pr.state,
                                        url: pr.html_url,
                                        is_draft: pr.draft,
                                        head_ref_name: pr.head.ref_name,
                                        base_ref_name: pr.base.ref_name,
                                        mergeable: "".into(),
                                        merge_state_status: "".into(),
                                        review_decision: None,
                                        status_check_rollup: vec![],
                                        checks: ChecksSummary::default(),
                                    }))
                                }
                            } else {
                                PanelState::Pr(Box::new(PrStatus {
                                    number: pr.number,
                                    title: pr.title,
                                    state: pr.state,
                                    url: pr.html_url,
                                    is_draft: pr.draft,
                                    head_ref_name: pr.head.ref_name,
                                    base_ref_name: pr.base.ref_name,
                                    mergeable: "".into(),
                                    merge_state_status: "".into(),
                                    review_decision: None,
                                    status_check_rollup: vec![],
                                    checks: ChecksSummary::default(),
                                }))
                            }
                        } else {
                            PanelState::NoPr
                        }
                    }
                    Err(e) => PanelState::Error {
                        message: format!("parse error: {e}"),
                    },
                }
            }
            Err(ForgeError::NotInstalled) => PanelState::NoGh,
            Err(ForgeError::NotAuthenticated) => PanelState::NotAuthenticated,
            Err(ForgeError::NoPr) => PanelState::NoPr,
            Err(ForgeError::RateLimited) => PanelState::RateLimited,
            Err(ForgeError::Other(m)) => PanelState::Error { message: m },
        };

        PrPanel {
            state,
            worktree: loc.path(),
            branch,
            fetched_at: crate::util::now(),
        }
    }

    fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, ForgeError> {
        let remote_url = loc
            .git_out(&["config", "--get", "remote.origin.url"])
            .ok_or_else(|| ForgeError::Other("No remote origin".into()))?;
        let (owner, repo) = self.get_owner_repo(&remote_url)?;
        let branch = self.get_branch(loc)?;

        // Get base branch (default to main or master)
        let base = opts.base.clone().unwrap_or_else(|| {
            // Try to detect from git config
            loc.git_out(&["config", "--get", "init.defaultBranch"])
                .unwrap_or_else(|| "main".into())
        });

        let title = opts.title.clone().unwrap_or_else(|| branch.clone());
        let body = opts.body.clone().unwrap_or_default();

        let endpoint = format!("/repos/{}/{}/pulls", owner, repo);
        let payload = serde_json::json!({
            "title": title,
            "body": body,
            "head": branch,
            "base": base,
            "draft": opts.draft,
        });

        // Use curl with JSON payload
        let token = self.get_token(loc)?;
        let remote_url = loc
            .git_out(&["config", "--get", "remote.origin.url"])
            .unwrap_or_default();
        let api_base = self.get_api_base(&remote_url)?;
        let url = format!("{}{}", api_base, endpoint);

        let out = std::process::Command::new("curl")
            .args([
                "-s",
                "-X",
                "POST",
                "-H",
                &format!("Authorization: token {}", token),
                "-H",
                "Content-Type: application/json",
                "-d",
                &payload.to_string(),
                &url,
            ])
            .output()
            .map_err(|e| ForgeError::Other(e.to_string()))?;

        if out.status.success() {
            let response = String::from_utf8_lossy(&out.stdout);
            // Try to extract URL from response
            if let Ok(pr) = serde_json::from_str::<serde_json::Value>(&response) {
                if let Some(url) = pr.get("html_url").and_then(|v| v.as_str()) {
                    return Ok(url.to_string());
                }
            }
            Ok(response.trim().to_string())
        } else {
            Err(classify_forgejo_error(
                &String::from_utf8_lossy(&out.stderr).to_lowercase(),
            ))
        }
    }

    fn open_pr(&self, loc: &GitLoc) -> Result<(), ForgeError> {
        let panel = self.pr_status(loc);
        match panel.state {
            PanelState::Pr(pr) => {
                // Open URL in browser
                std::process::Command::new("xdg-open")
                    .arg(&pr.url)
                    .spawn()
                    .map_err(|e| ForgeError::Other(e.to_string()))?;
                Ok(())
            }
            _ => Err(ForgeError::NoPr),
        }
    }

    fn approve_pr(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), ForgeError> {
        let panel = self.pr_status(loc);
        match panel.state {
            PanelState::Pr(pr) => {
                let remote_url = loc
                    .git_out(&["config", "--get", "remote.origin.url"])
                    .ok_or_else(|| ForgeError::Other("No remote origin".into()))?;
                let (owner, repo) = self.get_owner_repo(&remote_url)?;
                let endpoint = format!("/repos/{}/{}/pulls/{}/reviews", owner, repo, pr.number);

                let payload = serde_json::json!({
                    "event": "APPROVE",
                    "body": body.unwrap_or(""),
                });

                let token = self.get_token(loc)?;
                let api_base = self.get_api_base(&remote_url)?;
                let url = format!("{}{}", api_base, endpoint);

                let out = std::process::Command::new("curl")
                    .args([
                        "-s",
                        "-X",
                        "POST",
                        "-H",
                        &format!("Authorization: token {}", token),
                        "-H",
                        "Content-Type: application/json",
                        "-d",
                        &payload.to_string(),
                        &url,
                    ])
                    .output()
                    .map_err(|e| ForgeError::Other(e.to_string()))?;

                if out.status.success() {
                    Ok(())
                } else {
                    Err(classify_forgejo_error(
                        &String::from_utf8_lossy(&out.stderr).to_lowercase(),
                    ))
                }
            }
            _ => Err(ForgeError::NoPr),
        }
    }

    fn merge_pr(
        &self,
        loc: &GitLoc,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), ForgeError> {
        let panel = self.pr_status(loc);
        match panel.state {
            PanelState::Pr(pr) => {
                let remote_url = loc
                    .git_out(&["config", "--get", "remote.origin.url"])
                    .ok_or_else(|| ForgeError::Other("No remote origin".into()))?;
                let (owner, repo) = self.get_owner_repo(&remote_url)?;
                let endpoint = format!("/repos/{}/{}/pulls/{}/merge", owner, repo, pr.number);

                let merge_method = match method {
                    MergeMethod::Squash => "squash",
                    MergeMethod::Merge => "merge",
                    MergeMethod::Rebase => "rebase",
                };

                let payload = serde_json::json!({
                    "merge_method": merge_method,
                    "delete_branch_after_merge": delete_branch,
                    "force_merge": auto,
                });

                let token = self.get_token(loc)?;
                let api_base = self.get_api_base(&remote_url)?;
                let url = format!("{}{}", api_base, endpoint);

                let out = std::process::Command::new("curl")
                    .args([
                        "-s",
                        "-X",
                        "POST",
                        "-H",
                        &format!("Authorization: token {}", token),
                        "-H",
                        "Content-Type: application/json",
                        "-d",
                        &payload.to_string(),
                        &url,
                    ])
                    .output()
                    .map_err(|e| ForgeError::Other(e.to_string()))?;

                if out.status.success() {
                    Ok(())
                } else {
                    Err(classify_forgejo_error(
                        &String::from_utf8_lossy(&out.stderr).to_lowercase(),
                    ))
                }
            }
            _ => Err(ForgeError::NoPr),
        }
    }

    fn reviews(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        let panel = self.pr_status(loc);
        match panel.state {
            PanelState::Pr(pr) => {
                let remote_url = loc
                    .git_out(&["config", "--get", "remote.origin.url"])
                    .ok_or_else(|| ForgeError::Other("No remote origin".into()))?;
                let (owner, repo) = self.get_owner_repo(&remote_url)?;
                let endpoint = format!("/repos/{}/{}/pulls/{}/reviews", owner, repo, pr.number);

                let token = self.get_token(loc)?;
                let api_base = self.get_api_base(&remote_url)?;
                let url = format!("{}{}", api_base, endpoint);

                let out = std::process::Command::new("curl")
                    .args([
                        "-s",
                        "-X",
                        "GET",
                        "-H",
                        &format!("Authorization: token {}", token),
                        "-H",
                        "Accept: application/json",
                        &url,
                    ])
                    .output()
                    .map_err(|e| ForgeError::Other(e.to_string()))?;

                if out.status.success() {
                    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
                } else {
                    Err(classify_forgejo_error(
                        &String::from_utf8_lossy(&out.stderr).to_lowercase(),
                    ))
                }
            }
            _ => Err(ForgeError::NoPr),
        }
    }

    fn rerun_failed_checks(&self, loc: &GitLoc) -> Result<u32, ForgeError> {
        // Forgejo doesn't have a direct API to rerun checks like GitHub
        // This would require integration with the CI system (GitHub Actions, etc.)
        // For now, return 0 as a placeholder
        Ok(0)
    }
}

// --- API response models ----------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForgejoPullRequest {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    draft: bool,
    head: ForgejoHead,
    base: ForgejoBase,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForgejoPullRequestDetail {
    number: u64,
    title: String,
    state: String,
    html_url: String,
    draft: bool,
    head: ForgejoHead,
    base: ForgejoBase,
    #[serde(default)]
    mergeable: bool,
}

#[derive(Debug, Deserialize)]
struct ForgejoHead {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Debug, Deserialize)]
struct ForgejoBase {
    #[serde(rename = "ref")]
    ref_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForgejoCombinedStatus {
    state: String,
    #[serde(default)]
    statuses: Vec<CheckRun>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forgejo_forge_can_be_created() {
        let _forge = ForgejoForge;
        // Just verify it can be instantiated
    }

    #[test]
    fn api_base_detection_codeberg() {
        let forge = ForgejoForge;
        let url = "git@codeberg.org:owner/repo.git";
        let base = forge.get_api_base(url).unwrap();
        assert!(base.contains("codeberg.org"));
    }

    #[test]
    fn api_base_detection_https() {
        let forge = ForgejoForge;
        let url = "https://codeberg.org/owner/repo.git";
        let base = forge.get_api_base(url).unwrap();
        assert!(base.contains("codeberg.org"));
    }

    #[test]
    fn owner_repo_parsing_ssh() {
        let forge = ForgejoForge;
        let (owner, repo) = forge
            .get_owner_repo("git@codeberg.org:owner/repo.git")
            .unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn owner_repo_parsing_https() {
        let forge = ForgejoForge;
        let (owner, repo) = forge
            .get_owner_repo("https://codeberg.org/owner/repo.git")
            .unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }
}
