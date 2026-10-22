//! `thegn pr <action>` — pull-request data + actions for a worktree, through
//! the forge seam (`thegn_core::forge::Forge`, resolved per worktree by the
//! process's `ForgeSet`).
//!
//! `status` prints a human summary (and warms the panel cache the native host
//! reads); the native host refreshes the same feed in-process
//! (`hydrate.rs` `spawn_pr_cache_refresh`).

use anyhow::Result;
use thegn_core::db::Db;
use thegn_core::forge::model::{CreateOpts, MergeMethod, PanelState, PrPanel, ReviewState};
use thegn_core::forge::{Forge, ForgeError, PrDepth, PrRef};
use thegn_core::remote::GitLoc;
use thegn_core::store::CacheStore;
use thegn_core::{msg, outln};

use crate::cmd::{confirm, resolve_worktree};

/// PR subcommands, mirroring the user-facing half of the legacy `PrAction`
/// (the plugin-only `watch` + `--json` feeds were dropped with the panel WASM).
#[derive(clap::Subcommand, Clone)]
pub enum Action {
    /// PR + checks + review state (human summary).
    Status {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Create a PR for the worktree's branch.
    Create {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        draft: bool,
        #[arg(long)]
        web: bool,
        #[arg(long)]
        fill: bool,
    },
    /// Open the PR in a browser.
    Open {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Approve the PR.
    Approve {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        body: Option<String>,
    },
    /// Merge the PR.
    Merge {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long, value_enum, default_value_t = MergeMethod::Squash)]
        method: MergeMethod,
        #[arg(long)]
        delete_branch: bool,
        #[arg(long)]
        auto: bool,
        /// Skip the confirmation prompt (required for non-interactive/CI use).
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Re-run failed checks.
    RerunChecks {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Print the PR's reviews as JSON.
    Reviews {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
    },
    /// Post a PR-level comment.
    Comment {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        body: String,
    },
    /// Submit a review (approve / request-changes / comment).
    Review {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long, value_enum)]
        state: ReviewState,
        /// Required for request-changes / comment.
        #[arg(long)]
        body: Option<String>,
    },
    /// Print the PR's unified diff (or `--json` for the parsed structure).
    Diff {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        #[arg(long)]
        json: bool,
    },
    /// Mark the PR ready for review (or `--undo` back to draft).
    Ready {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Convert the PR back to a draft instead of marking it ready.
        #[arg(long)]
        undo: bool,
    },
    /// Enable (or `--disable`) auto-merge for the PR.
    AutoMerge {
        #[command(flatten)]
        target: super::target::WorktreeFlag,
        /// Disable auto-merge instead of enabling it.
        #[arg(long)]
        disable: bool,
    },
    /// The PR queue: shepherd pull requests on the forge — poll them, classify
    /// what is blocking them, optionally hand a blocker to an agent, and let the
    /// forge merge them once green (`[pr_queue]`).
    Queue {
        #[command(subcommand)]
        action: super::pr_queue::Action,
    },
}

pub fn run(cfg: &thegn_core::config::Config, action: Action) -> Result<()> {
    match action {
        // The only verb needing config: the queue is configurable per repo.
        Action::Queue { action } => super::pr_queue::run(cfg, action),
        Action::Status { target } => status(target.worktree),
        Action::Create {
            target,
            title,
            body,
            base,
            draft,
            web,
            fill,
        } => create(target.worktree, title, body, base, draft, web, fill),
        Action::Open { target } => open(target.worktree),
        Action::Approve { target, body } => approve(target.worktree, body),
        Action::Merge {
            target,
            method,
            delete_branch,
            auto,
            yes,
        } => merge(target.worktree, method, delete_branch, auto, yes),
        Action::RerunChecks { target } => rerun(target.worktree),
        Action::Reviews { target } => reviews(target.worktree),
        Action::Comment { target, body } => comment(target.worktree, body),
        Action::Review {
            target,
            state,
            body,
        } => review(target.worktree, state, body),
        Action::Diff { target, json } => diff(target.worktree, json),
        Action::Ready { target, undo } => ready(target.worktree, undo),
        Action::AutoMerge { target, disable } => auto_merge(target.worktree, disable),
    }
}

/// The process's forge set — each verb routes its worktree through `for_loc`.
fn forges() -> std::sync::Arc<thegn_svc::forge::ForgeSet> {
    crate::forge_handle::get()
}

fn status(worktree: Option<String>) -> Result<()> {
    let wt = resolve_worktree(worktree);
    let loc = GitLoc::for_worktree(&wt);
    let panel = forges()
        .for_loc(&loc)
        .pr_panel(&loc, PrRef::Current, PrDepth::Header);
    let json = serde_json::to_string(&panel).unwrap_or_default();
    if let Ok(db) = Db::open() {
        // Host-path cache key — matches the panel's reader (never `loc.path()`).
        let _ = db.put_pr_cache(&GitLoc::worktree_cache_key(&wt), &panel.branch, &json); // best-effort: cache write: the DB is a cache; git/forge stays the source of truth
    }
    print_summary(&panel);
    Ok(())
}

fn print_summary(p: &PrPanel) {
    match &p.state {
        PanelState::NoGh => outln!("forge CLI not installed (gh)"),
        PanelState::NotAuthenticated => outln!("forge not authenticated (run: gh auth login)"),
        PanelState::NoPr => outln!(
            "branch '{}': no PR yet  (create: thegn pr create)",
            p.branch
        ),
        PanelState::RateLimited => outln!("forge API rate limited; try again shortly"),
        PanelState::Offline => outln!("forge unreachable (network error)"),
        PanelState::Error { message } => outln!("error: {message}"),
        PanelState::Pr(pr) => {
            let draft = if pr.is_draft { " (draft)" } else { "" };
            outln!("#{} {}{}  [{}]", pr.number, pr.title, draft, pr.state);
            outln!(
                "  checks: {} ok / {} failed / {} pending   review: {}",
                pr.checks.passed,
                pr.checks.failed,
                pr.checks.pending,
                pr.review_decision.as_deref().unwrap_or("—")
            );
            outln!("  {}", pr.url);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create(
    worktree: Option<String>,
    title: Option<String>,
    body: Option<String>,
    base: Option<String>,
    draft: bool,
    web: bool,
    fill: bool,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let fill = fill || (title.is_none() && body.is_none() && !web);
    let opts = CreateOpts {
        title,
        body,
        base,
        draft,
        web,
        fill,
    };
    match forges().for_loc(&loc).create_pr(&loc, &opts) {
        Ok(out) => {
            if !out.is_empty() {
                outln!("{out}");
            }
            msg::info("PR created");
        }
        Err(e) => msg::die(&format!("pr create failed: {}", e.describe())),
    }
    Ok(())
}

fn open(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if let Err(e) = forges().for_loc(&loc).open_in_browser(&loc, None) {
        msg::die(&format!("pr open failed: {}", e.describe()));
    }
    Ok(())
}

fn approve(worktree: Option<String>, body: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let registry = forges();
    let forge = registry.for_loc(&loc);
    let pr = match forge.pr_status(&loc, PrRef::Current) {
        Ok(pr) => pr,
        Err(e) => msg::die(&format!(
            "pr approve failed: could not read the PR: {}",
            e.describe()
        )),
    };
    match review_reviewed_head(
        forge,
        &loc,
        &ReviewedPr {
            number: pr.number,
            head: &pr.head_ref_oid,
        },
        ReviewState::Approve,
        body.as_deref(),
    ) {
        Ok(()) => msg::info(&format!(
            "PR #{} approved at {}",
            pr.number,
            pr.head_ref_oid.chars().take(12).collect::<String>()
        )),
        Err(e) => msg::die(&format!("pr approve failed: {}", e.describe())),
    }
    Ok(())
}

fn merge(
    worktree: Option<String>,
    method: MergeMethod,
    delete_branch: bool,
    auto: bool,
    yes: bool,
) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let forges = forges();
    let forge = forges.for_loc(&loc);
    let pr = match forge.pr_status(&loc, PrRef::Current) {
        Ok(pr) => pr,
        Err(e) => msg::die(&format!(
            "pr merge failed: could not read the PR: {}",
            e.describe()
        )),
    };
    let short_head = pr.head_ref_oid.chars().take(12).collect::<String>();
    // Confirmation is required unless `--yes` is passed. Without a TTY (CI /
    // piped scripts) a stdin prompt can't be answered, so refuse with a
    // non-zero exit rather than silently cancelling and reporting success —
    // scripts branch on the exit code.
    if !yes {
        let prompt = format!("Merge PR #{} at {} ({method:?})?", pr.number, short_head);
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!("{prompt} refusing to merge without confirmation — pass --yes");
        }
        if !confirm(&prompt) {
            anyhow::bail!("merge cancelled");
        }
    }
    let outcome = merge_reviewed_head(
        forge,
        &loc,
        &ReviewedPr {
            number: pr.number,
            head: &pr.head_ref_oid,
        },
        method,
        delete_branch,
        auto,
    );
    match merge_outcome_text(&outcome) {
        Ok(text) => msg::info(&text),
        Err(text) => msg::die(&format!("pr merge failed: {text}")),
    }
    Ok(())
}

pub(crate) struct ReviewedPr<'a> {
    pub number: u64,
    pub head: &'a str,
}

pub(crate) fn merge_reviewed_head(
    forge: &dyn Forge,
    loc: &GitLoc,
    reviewed: &ReviewedPr<'_>,
    method: MergeMethod,
    delete_branch: bool,
    auto: bool,
) -> thegn_core::pr_merge_outcome::MergeOutcome {
    use thegn_core::pr_merge_outcome::{MergeOutcome, classify_merge};

    if reviewed.number == 0
        || !matches!(reviewed.head.len(), 40 | 64)
        || !reviewed.head.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return MergeOutcome::Rejected("PR head unknown; refresh before merging".into());
    }

    let pr = PrRef::Number(reviewed.number);
    let result = forge.merge_pr(loc, pr, method, delete_branch, Some(reviewed.head), auto);
    let after = match forge.pr_status(loc, pr) {
        Ok(status) if status.number == reviewed.number => Some(status),
        Ok(_) | Err(_) => None,
    };
    classify_merge(!auto, &result, after.as_ref(), Some(reviewed.head))
}

/// Re-attest the displayed PR number and head before submitting a review.
/// The re-read narrows but does not close the window between the read and the
/// forge accepting the review; a commit-pinned review API call is a later slice.
pub(crate) fn review_reviewed_head(
    forge: &dyn Forge,
    loc: &GitLoc,
    reviewed: &ReviewedPr<'_>,
    state: ReviewState,
    body: Option<&str>,
) -> std::result::Result<(), ForgeError> {
    if reviewed.number == 0
        || !matches!(reviewed.head.len(), 40 | 64)
        || !reviewed.head.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ForgeError::Other(
            "PR head unknown; refresh before reviewing".into(),
        ));
    }

    let pr = PrRef::Number(reviewed.number);
    let current = forge.pr_status(loc, pr)?;
    if current.number != reviewed.number {
        return Err(ForgeError::Other(format!(
            "forge returned PR #{} for #{}; nothing was submitted",
            current.number, reviewed.number
        )));
    }
    if !current.head_ref_oid.eq_ignore_ascii_case(reviewed.head) {
        let old = reviewed.head.chars().take(12).collect::<String>();
        let new = current.head_ref_oid.chars().take(12).collect::<String>();
        return Err(ForgeError::Other(format!(
            "PR #{} moved from {old} to {new} since it was shown; refresh and review again — nothing was submitted",
            reviewed.number
        )));
    }
    forge.submit_review(loc, pr, state, body)
}

pub(crate) fn merge_outcome_text(
    outcome: &thegn_core::pr_merge_outcome::MergeOutcome,
) -> std::result::Result<String, String> {
    use thegn_core::pr_merge_outcome::MergeOutcome;

    match outcome {
        MergeOutcome::Confirmed => Ok("PR merged".into()),
        MergeOutcome::Requested => Ok("merge requested; the forge has not confirmed it yet".into()),
        MergeOutcome::AutoMergeArmed => {
            Ok("auto-merge armed; the forge merges when its rules allow".into())
        }
        MergeOutcome::StaleHead { observed } => Err(format!(
            "PR head moved to {} after it was reviewed; nothing was merged. Refresh and confirm again",
            observed.chars().take(12).collect::<String>()
        )),
        MergeOutcome::Rejected(reason) => Err(format!("merge refused: {reason}")),
        MergeOutcome::Unknown(reason) => Err(format!(
            "merge result unknown ({reason}); check the PR before retrying"
        )),
    }
}

fn rerun(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).rerun_failed(&loc, PrRef::Current) {
        Ok(0) => msg::info("no failed checks to re-run"),
        Ok(n) => msg::info(&format!("re-ran {n} failed workflow run(s)")),
        Err(e) => msg::die(&format!("pr rerun-checks failed: {}", e.describe())),
    }
    Ok(())
}

fn reviews(worktree: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).reviews_json(&loc) {
        Ok(json) => outln!("{json}"),
        Err(e) => msg::die(&format!("pr reviews failed: {}", e.describe())),
    }
    Ok(())
}

fn comment(worktree: Option<String>, body: String) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges().for_loc(&loc).comment(&loc, PrRef::Current, &body) {
        Ok(()) => msg::info("comment posted"),
        Err(e) => msg::die(&format!("pr comment failed: {}", e.describe())),
    }
    Ok(())
}

fn review(worktree: Option<String>, state: ReviewState, body: Option<String>) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    let registry = forges();
    let forge = registry.for_loc(&loc);
    let pr = match forge.pr_status(&loc, PrRef::Current) {
        Ok(pr) => pr,
        Err(e) => msg::die(&format!(
            "pr review failed: could not read the PR: {}",
            e.describe()
        )),
    };
    match review_reviewed_head(
        forge,
        &loc,
        &ReviewedPr {
            number: pr.number,
            head: &pr.head_ref_oid,
        },
        state,
        body.as_deref(),
    ) {
        Ok(()) => msg::info(&format!(
            "review submitted on PR #{} at {}",
            pr.number,
            pr.head_ref_oid.chars().take(12).collect::<String>()
        )),
        Err(e) => msg::die(&format!("pr review failed: {}", e.describe())),
    }
    Ok(())
}

fn diff(worktree: Option<String>, json: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    if json {
        match forges().for_loc(&loc).pr_diff(&loc, PrRef::Current) {
            Ok(d) => outln!("{}", serde_json::to_string_pretty(&d).unwrap_or_default()),
            Err(e) => msg::die(&format!("pr diff failed: {}", e.describe())),
        }
    } else {
        // Raw unified diff text from the forge.
        match forges().for_loc(&loc).pr_diff_raw(&loc, PrRef::Current) {
            Ok(raw) => outln!("{raw}"),
            Err(e) => msg::die(&format!("pr diff failed: {}", e.describe())),
        }
    }
    Ok(())
}

fn ready(worktree: Option<String>, undo: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    // `undo` converts the PR back to a draft; otherwise mark it ready.
    match forges().for_loc(&loc).set_draft(&loc, PrRef::Current, undo) {
        Ok(()) if undo => msg::info("PR converted to draft"),
        Ok(()) => msg::info("PR marked as ready for review"),
        Err(e) => msg::die(&format!("pr ready failed: {}", e.describe())),
    }
    Ok(())
}

fn auto_merge(worktree: Option<String>, disable: bool) -> Result<()> {
    let loc = GitLoc::for_worktree(&resolve_worktree(worktree));
    match forges()
        .for_loc(&loc)
        .set_auto_merge(&loc, PrRef::Current, !disable, MergeMethod::Squash)
    {
        Ok(()) if disable => msg::info("auto-merge disabled"),
        Ok(()) => msg::info("auto-merge enabled"),
        Err(e) => msg::die(&format!("pr auto-merge failed: {}", e.describe())),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ReviewedPr, merge_outcome_text, merge_reviewed_head, review_reviewed_head};
    use std::sync::Mutex;
    use thegn_core::forge::model::{MergeMethod, PrHeader, PrStatus, ReviewState};
    use thegn_core::forge::{Forge, ForgeCaps, ForgeError, PrRef};
    use thegn_core::remote::GitLoc;
    use thegn_core::seam::{Availability, Probe, ProbeReport};

    struct FakeForge {
        result: Mutex<std::result::Result<(), ForgeError>>,
        after: Mutex<std::result::Result<PrStatus, ForgeError>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeForge {
        fn new(
            result: std::result::Result<(), ForgeError>,
            after: std::result::Result<PrStatus, ForgeError>,
        ) -> Self {
            Self {
                result: Mutex::new(result),
                after: Mutex::new(after),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl Probe for FakeForge {
        fn probe(&self) -> ProbeReport {
            ProbeReport::new("forge", "fake", Availability::Ready)
        }
    }

    impl Forge for FakeForge {
        fn id(&self) -> &'static str {
            "fake"
        }

        fn caps(&self) -> ForgeCaps {
            ForgeCaps::ALL
        }

        fn repo_ref(&self, _: &GitLoc) -> Option<thegn_core::forge::RepoRef> {
            None
        }

        fn pr_status(&self, _: &GitLoc, pr: PrRef) -> std::result::Result<PrStatus, ForgeError> {
            self.calls.lock().unwrap().push(format!("status {pr:?}"));
            self.after.lock().unwrap().clone()
        }

        fn pr_list(&self, _: &GitLoc, _: usize) -> std::result::Result<Vec<PrHeader>, ForgeError> {
            Ok(Vec::new())
        }

        fn merge_pr(
            &self,
            _: &GitLoc,
            pr: PrRef,
            method: MergeMethod,
            delete_branch: bool,
            expected_head: Option<&str>,
            auto: bool,
        ) -> std::result::Result<(), ForgeError> {
            self.calls.lock().unwrap().push(format!(
                "merge {pr:?} {method:?} {delete_branch} {expected_head:?} {auto}"
            ));
            self.result.lock().unwrap().clone()
        }

        fn submit_review(
            &self,
            _: &GitLoc,
            pr: PrRef,
            state: ReviewState,
            body: Option<&str>,
        ) -> std::result::Result<(), ForgeError> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("review {pr:?} {state:?} {body:?}"));
            self.result.lock().unwrap().clone()
        }
    }

    fn status(number: u64, state: &str, head: &str) -> PrStatus {
        PrStatus {
            number,
            state: state.into(),
            head_ref_oid: head.into(),
            ..PrStatus::default()
        }
    }

    fn loc() -> GitLoc {
        GitLoc::Local(std::path::PathBuf::from("/tmp/pr-merge-reviewed-head"))
    }

    fn reviewed(number: u64) -> ReviewedPr<'static> {
        ReviewedPr {
            number,
            head: "0123456789abcdef0123456789abcdef01234567",
        }
    }

    #[test]
    fn review_reviewed_head_submits_by_number_when_the_head_matches() {
        let forge = FakeForge::new(Ok(()), Ok(status(7, "OPEN", reviewed(7).head)));
        assert!(
            review_reviewed_head(&forge, &loc(), &reviewed(7), ReviewState::Approve, None,).is_ok()
        );
        assert_eq!(
            forge.calls(),
            vec!["status Number(7)", "review Number(7) Approve None"]
        );
    }

    #[test]
    fn review_reviewed_head_refuses_a_moved_head_without_submitting() {
        let forge = FakeForge::new(
            Ok(()),
            Ok(status(
                7,
                "OPEN",
                "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
            )),
        );
        let error = review_reviewed_head(&forge, &loc(), &reviewed(7), ReviewState::Approve, None)
            .unwrap_err();
        assert!(error.describe().contains("abcdefabcdef"));
        assert!(error.describe().contains("nothing was submitted"));
        assert_eq!(forge.calls(), vec!["status Number(7)"]);
    }

    #[test]
    fn review_reviewed_head_refuses_unknown_head_without_calling_the_forge() {
        for head in ["", "1234567", "0123456789abcdef0123456789abcdef0123456g"] {
            let forge = FakeForge::new(Ok(()), Ok(status(7, "OPEN", head)));
            let reviewed = ReviewedPr { number: 7, head };
            assert!(
                review_reviewed_head(&forge, &loc(), &reviewed, ReviewState::Approve, None,)
                    .is_err()
            );
            assert!(forge.calls().is_empty());
        }
        let forge = FakeForge::new(Ok(()), Ok(status(0, "OPEN", reviewed(0).head)));
        assert!(
            review_reviewed_head(&forge, &loc(), &reviewed(0), ReviewState::Approve, None,)
                .is_err()
        );
        assert!(forge.calls().is_empty());
    }

    #[test]
    fn review_reviewed_head_refuses_a_different_pr_number() {
        let forge = FakeForge::new(Ok(()), Ok(status(8, "OPEN", reviewed(7).head)));
        let error = review_reviewed_head(&forge, &loc(), &reviewed(7), ReviewState::Approve, None)
            .unwrap_err();
        assert!(error.describe().contains("PR #8 for #7"));
        assert!(error.describe().contains("nothing was submitted"));
        assert_eq!(forge.calls(), vec!["status Number(7)"]);
    }

    #[test]
    fn merge_reviewed_head_passes_the_pr_number_and_expected_head() {
        let forge = FakeForge::new(Ok(()), Ok(status(7, "MERGED", reviewed(7).head)));
        let outcome = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            true,
            false,
        );
        assert_eq!(
            forge.calls(),
            vec![
                "merge Number(7) Squash true Some(\"0123456789abcdef0123456789abcdef01234567\") false",
                "status Number(7)"
            ]
        );
        assert_eq!(
            outcome,
            thegn_core::pr_merge_outcome::MergeOutcome::Confirmed
        );
    }

    #[test]
    fn moved_head_is_stale_and_never_reported_merged() {
        let forge = FakeForge::new(
            Err(ForgeError::Other("head mismatch".into())),
            Ok(status(
                7,
                "OPEN",
                "abcdefabcdefabcdefabcdefabcdefabcdefabcd",
            )),
        );
        let outcome = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            false,
            false,
        );
        assert!(matches!(
            &outcome,
            thegn_core::pr_merge_outcome::MergeOutcome::StaleHead { .. }
        ));
        let text = merge_outcome_text(&outcome).unwrap_err();
        assert!(text.contains("abcdefabcdef"), "{text}");
        assert!(text.contains("nothing was merged"), "{text}");
    }

    #[test]
    fn unknown_or_short_head_refuses_without_calling_the_forge() {
        for head in ["", "1234567", "0123456789abcdef0123456789abcdef0123456g"] {
            let forge = FakeForge::new(Ok(()), Ok(status(7, "MERGED", head)));
            let reviewed = ReviewedPr { number: 7, head };
            assert!(matches!(
                merge_reviewed_head(&forge, &loc(), &reviewed, MergeMethod::Squash, false, false),
                thegn_core::pr_merge_outcome::MergeOutcome::Rejected(_)
            ));
            assert!(forge.calls().is_empty());
        }
        let forge = FakeForge::new(Ok(()), Ok(status(0, "MERGED", reviewed(0).head)));
        assert!(matches!(
            merge_reviewed_head(
                &forge,
                &loc(),
                &reviewed(0),
                MergeMethod::Squash,
                false,
                false
            ),
            thegn_core::pr_merge_outcome::MergeOutcome::Rejected(_)
        ));
        assert!(forge.calls().is_empty());
    }

    #[test]
    fn armed_and_requested_are_not_reported_merged() {
        let forge = FakeForge::new(Ok(()), Ok(status(7, "OPEN", reviewed(7).head)));
        let armed = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            false,
            true,
        );
        assert_eq!(
            merge_outcome_text(&armed).unwrap(),
            "auto-merge armed; the forge merges when its rules allow"
        );

        let forge = FakeForge::new(Ok(()), Ok(status(7, "OPEN", reviewed(7).head)));
        let requested = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            false,
            false,
        );
        assert_eq!(
            merge_outcome_text(&requested).unwrap(),
            "merge requested; the forge has not confirmed it yet"
        );
    }

    #[test]
    fn confirmed_merge_reads_merged() {
        let forge = FakeForge::new(Ok(()), Ok(status(7, "MERGED", reviewed(7).head)));
        let outcome = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            false,
            false,
        );
        assert_eq!(
            outcome,
            thegn_core::pr_merge_outcome::MergeOutcome::Confirmed
        );
        assert_eq!(merge_outcome_text(&outcome).unwrap(), "PR merged");
    }

    #[test]
    fn offline_merge_without_a_read_is_unknown() {
        let forge = FakeForge::new(Err(ForgeError::Offline), Err(ForgeError::Offline));
        let outcome = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            false,
            false,
        );
        assert!(matches!(
            &outcome,
            thegn_core::pr_merge_outcome::MergeOutcome::Unknown(_)
        ));
        assert!(
            merge_outcome_text(&outcome)
                .unwrap_err()
                .contains("check the PR")
        );
    }

    #[test]
    fn a_follow_up_read_of_another_pr_is_not_evidence() {
        let forge = FakeForge::new(Ok(()), Ok(status(8, "MERGED", reviewed(7).head)));
        let outcome = merge_reviewed_head(
            &forge,
            &loc(),
            &reviewed(7),
            MergeMethod::Squash,
            false,
            false,
        );
        assert_eq!(
            outcome,
            thegn_core::pr_merge_outcome::MergeOutcome::Requested
        );
        assert_eq!(
            forge.calls().last().map(String::as_str),
            Some("status Number(7)")
        );
    }
}
