pub mod models;
pub use models::*;

use crate::github::GitHubForge;
use crate::forgejo::ForgejoForge;
use crate::remote::GitLoc;

pub trait Forge {
    fn pr_status(&self, loc: &GitLoc) -> PrPanel;
    fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, ForgeError>;
    fn open_pr(&self, loc: &GitLoc) -> Result<(), ForgeError>;
    fn approve_pr(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), ForgeError>;
    fn merge_pr(
        &self,
        loc: &GitLoc,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), ForgeError>;
    fn reviews(&self, loc: &GitLoc) -> Result<String, ForgeError>;
    fn rerun_failed_checks(&self, loc: &GitLoc) -> Result<u32, ForgeError>;
    fn set_draft(&self, loc: &GitLoc, draft: bool) -> Result<(), ForgeError>;
    fn set_auto_merge(&self, loc: &GitLoc, enable: bool) -> Result<(), ForgeError>;

    fn list_issues(&self, loc: &GitLoc, state: &str) -> Result<Vec<Issue>, ForgeError>;
    fn get_issue(&self, loc: &GitLoc, issue: u64) -> Result<Issue, ForgeError>;
    fn create_issue(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<Issue, ForgeError>;
    fn issue_comment(&self, loc: &GitLoc, issue: u64, body: &str) -> Result<(), ForgeError>;

    fn get_check_logs(&self, loc: &GitLoc, check_name: &str) -> Result<String, ForgeError>;
}

pub fn extract_issue_from_branch(branch: &str) -> Option<u64> {
    branch.split(|c: char| !c.is_numeric())
        .find(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

pub fn detect_forge_from_url(url: &str) -> Option<Box<dyn Forge>> {
    if url.contains("github.com") {
        Some(Box::new(GitHubForge))
    } else if url.contains("codeberg.org") || url.contains("forgejo") || url.contains("gitea") {
        Some(Box::new(ForgejoForge))
    } else {
        None
    }
}

pub fn get_forge_for_loc(loc: &GitLoc) -> Option<Box<dyn Forge>> {
    let url = loc
        .git_out(&["config", "--get", "remote.origin.url"])
        .unwrap_or_default();
    detect_forge_from_url(&url)
}
