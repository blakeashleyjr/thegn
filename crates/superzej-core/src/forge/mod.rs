pub mod models;
pub use models::*;

use crate::github::GitHubForge;
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
}

pub fn detect_forge_from_url(url: &str) -> Option<Box<dyn Forge>> {
    if url.contains("github.com") {
        Some(Box::new(GitHubForge))
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
