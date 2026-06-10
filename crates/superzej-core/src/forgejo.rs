use crate::forge::models::*;
use crate::forge::Forge;
use crate::remote::GitLoc;
use serde::Deserialize;

pub struct ForgejoForge;

impl ForgejoForge {
    fn api_request(&self, loc: &GitLoc, endpoint: &str, method: &str) -> Result<String, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
}

impl Forge for ForgejoForge {
    fn pr_status(&self, loc: &GitLoc) -> PrPanel {
        PrPanel {
            state: PanelState::Error { message: "Not implemented".into() },
            worktree: loc.path(),
            branch: "".into(),
            fetched_at: 0,
        }
    }
    fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn open_pr(&self, loc: &GitLoc) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn approve_pr(&self, loc: &GitLoc, body: Option<&str>) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn merge_pr(&self, loc: &GitLoc, method: MergeMethod, delete_branch: bool, auto: bool) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn reviews(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn rerun_failed_checks(&self, loc: &GitLoc) -> Result<u32, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn set_draft(&self, loc: &GitLoc, draft: bool) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn set_auto_merge(&self, loc: &GitLoc, enable: bool) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn list_issues(&self, loc: &GitLoc, state: &str) -> Result<Vec<Issue>, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn get_issue(&self, loc: &GitLoc, issue: u64) -> Result<Issue, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn create_issue(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<Issue, ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
    fn issue_comment(&self, loc: &GitLoc, issue: u64, body: &str) -> Result<(), ForgeError> {
        Err(ForgeError::Other("Not implemented".into()))
    }
}
