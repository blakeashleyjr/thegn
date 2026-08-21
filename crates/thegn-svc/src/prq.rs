//! The PR queue's forge seam.
//!
//! The queue driver must not call `thegn_core::github` directly: the roadmap's
//! multi-forge work (GitLab / Gitea / Forgejo) is coming, and a driver welded to
//! one provider would have to be rewritten for it. This is the **narrow** waist
//! — the six operations the queue actually performs — rather than the whole
//! forge abstraction, which stays a separate piece of work.
//!
//! [`GithubPrq`] implements it entirely by delegating to functions that already
//! shipped, so this adds a seam, not a second GitHub client.

use thegn_core::github::{self, GhError, MergeMethod, PanelState, PrStatus, ReviewThreadRow};
use thegn_core::remote::GitLoc;

/// The forge operations a PR queue needs.
///
/// Object-safe (`&self`, concrete args) so a driver can hold `&dyn PrQueueForge`
/// and a test can substitute a fake without touching the network.
pub trait PrQueueForge {
    /// Which forge this is, for the row's `forge` column.
    fn id(&self) -> &'static str;

    /// One pull request's current state, by number. Includes review threads,
    /// because "changes requested" is part of classification.
    fn fetch(&self, loc: &GitLoc, number: u64) -> Result<FetchedPr, GhError>;

    /// Ask the forge to merge this PR **itself** once its own rules allow.
    /// The default path: branch protection and required reviews stay in charge.
    fn enable_auto_merge(
        &self,
        loc: &GitLoc,
        number: u64,
        method: MergeMethod,
        delete_branch: bool,
    ) -> Result<(), GhError>;

    /// Merge it now. Only used when the user explicitly opts out of the forge's
    /// own gating with `merge_mode = "thegn"`.
    fn merge(
        &self,
        loc: &GitLoc,
        number: u64,
        method: MergeMethod,
        delete_branch: bool,
    ) -> Result<(), GhError>;

    /// Reply to a review thread. Note there is deliberately no `resolve`:
    /// marking feedback resolved is the reviewer's judgement, not thegn's.
    fn reply_to_thread(&self, loc: &GitLoc, thread_id: &str, body: &str) -> Result<(), GhError>;

    /// Re-run the failed checks for this PR's branch. Returns how many were
    /// restarted — the cheap fix to try before waking an agent, since plenty of
    /// red builds are flakes.
    fn rerun_failed(&self, loc: &GitLoc) -> Result<u32, GhError>;
}

/// One fetch's result: the PR plus the threads that came with it.
#[derive(Debug, Clone)]
pub struct FetchedPr {
    pub pr: PrStatus,
    pub threads: Vec<ReviewThreadRow>,
}

/// GitHub, via the shipped `gh`-backed helpers in `thegn_core::github`.
#[derive(Debug, Clone, Copy, Default)]
pub struct GithubPrq;

impl GithubPrq {
    pub fn new() -> Self {
        GithubPrq
    }
}

impl PrQueueForge for GithubPrq {
    fn id(&self) -> &'static str {
        "github"
    }

    fn fetch(&self, loc: &GitLoc, number: u64) -> Result<FetchedPr, GhError> {
        let panel = github::pr_status_with_threads(loc, number);
        // `pr_status_*` folds every failure into a PanelState rather than an
        // Err, so unwrap it back into the Result this trait promises — the
        // driver needs to tell "no PR" from "offline" to decide whether to back
        // off or drop the row.
        match panel.state {
            PanelState::Pr(pr) => Ok(FetchedPr {
                pr: *pr,
                threads: panel.threads,
            }),
            PanelState::NoGh => Err(GhError::NotInstalled),
            PanelState::NotAuthenticated => Err(GhError::NotAuthenticated),
            PanelState::NoPr => Err(GhError::NoPr),
            PanelState::RateLimited => Err(GhError::RateLimited),
            PanelState::Offline => Err(GhError::Offline),
            PanelState::Error { message } => Err(GhError::Other(message)),
        }
    }

    fn enable_auto_merge(
        &self,
        loc: &GitLoc,
        number: u64,
        method: MergeMethod,
        delete_branch: bool,
    ) -> Result<(), GhError> {
        // `github::set_auto_merge` hardcodes --squash; go through `merge_pr`
        // with `auto = true` so the configured method is honored.
        let _ = number;
        github::merge_pr(loc, method, delete_branch, true)
    }

    fn merge(
        &self,
        loc: &GitLoc,
        number: u64,
        method: MergeMethod,
        delete_branch: bool,
    ) -> Result<(), GhError> {
        let _ = number;
        github::merge_pr(loc, method, delete_branch, false)
    }

    fn reply_to_thread(&self, loc: &GitLoc, thread_id: &str, body: &str) -> Result<(), GhError> {
        github::reply_to_thread(loc, thread_id, body)
    }

    fn rerun_failed(&self, loc: &GitLoc) -> Result<u32, GhError> {
        github::rerun_failed_checks(loc)
    }
}

/// Resolve a forge implementation by id. One place to extend when GitLab and
/// friends land (roadmap AT 631).
pub fn for_id(id: &str) -> Box<dyn PrQueueForge + Send + Sync> {
    // Only GitHub today. Unknown ids deliberately fall back rather than failing:
    // a row written by a newer build must not brick an older build's drain.
    // This becomes a real match when the second provider lands.
    let _ = id;
    Box::new(GithubPrq::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_is_the_default_forge() {
        assert_eq!(GithubPrq::new().id(), "github");
        assert_eq!(for_id("github").id(), "github");
        // An unrecognized id degrades rather than panicking, so a row written by
        // a newer build cannot brick an older build's drain.
        assert_eq!(for_id("gitlab").id(), "github");
        assert_eq!(for_id("").id(), "github");
    }
}
