//! The forge seam: pull requests, reviews, checks, issues and identity on a
//! git hosting service, behind one object-safe trait.
//!
//! Shape (the provider-seams spec):
//!
//! - [`Forge`] is a **sync** seam — every implementation is process-bound
//!   (`gh`) or wraps its own async client — so host code calls it from a
//!   blocking thread (`sched::spawn_bg` / `spawn_blocking`) and the queue
//!   driver can hold a `&dyn Forge` on a plain CLI thread.
//! - Optional operations are declared by a [`ForgeCaps`] bit and default to
//!   `Err(ForgeError::Unsupported(op))`, so a thinner forge is an
//!   implementation, not a rewrite.
//! - [`ForgeError`] is the seam error (`SeamError`): a `Ladder` falls through
//!   `Unsupported`/`NotInstalled`/`NotConfigured` layers and stops on
//!   `Auth`/`NotFound`/`Transient`.
//! - [`model`] holds every type the host renders plus the pure parsers; the
//!   `pr_cache` wire shape ([`model::PrPanel`]) is produced only by
//!   [`model::PrPanel::from_result`] so transport errors and panel states
//!   never round-trip through each other.
//!
//! Implementations: [`crate::github::GithubCli`] (the `gh` CLI transport, in
//! core) and `thegn_svc::forge::GithubNative` (octocrab), composed by
//! `thegn_svc::forge::forges_for` into a per-host `ForgeSet`. Nothing outside
//! those files may name the GitHub CLI layer — the `forge-leak` ratchet in
//! `just lint` pins it.

pub mod model;

use crate::remote::GitLoc;
use crate::seam::{ErrorClass, Probe, SeamError};
use model::*;

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Distinguishable forge failure modes. The variant set is the `gh` CLI's
/// classification (it is what the panel renders), plus the seam's
/// `Unsupported`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForgeError {
    /// The provider's binary / client is absent.
    NotInstalled,
    /// Nothing configured for this layer (no token, remote location for a
    /// local-only client, circuit open). A ladder falls through.
    NotConfigured(&'static str),
    NotAuthenticated,
    /// No pull request (or target object) — a 404.
    NoPr,
    RateLimited,
    /// Transient network failure (DNS, TCP connect, TLS). Separate from `Other`
    /// so the UI can show "unreachable" and callers can circuit-break.
    Offline,
    /// The provider declares it cannot do this operation (caps bit off).
    Unsupported(&'static str),
    Other(String),
}

impl ForgeError {
    /// The user-facing one-liner (status line, `thegn pr` errors).
    pub fn describe(&self) -> String {
        match self {
            ForgeError::NotInstalled => "forge CLI not installed (gh)".into(),
            ForgeError::NotConfigured(what) => format!("forge not configured: {what}"),
            ForgeError::NotAuthenticated => "forge not authenticated (run: gh auth login)".into(),
            ForgeError::NoPr => "no PR for this branch".into(),
            ForgeError::RateLimited => "forge API rate limited".into(),
            ForgeError::Offline => "forge unreachable".into(),
            ForgeError::Unsupported(op) => format!("this forge does not support {op}"),
            ForgeError::Other(m) => m.clone(),
        }
    }
}

impl std::fmt::Display for ForgeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.describe())
    }
}

impl std::error::Error for ForgeError {}

impl SeamError for ForgeError {
    fn class(&self) -> ErrorClass {
        match self {
            ForgeError::NotInstalled => ErrorClass::NotInstalled,
            ForgeError::NotConfigured(_) => ErrorClass::NotConfigured,
            ForgeError::NotAuthenticated => ErrorClass::Auth,
            ForgeError::NoPr => ErrorClass::NotFound,
            ForgeError::RateLimited => ErrorClass::RateLimited,
            ForgeError::Offline => ErrorClass::Transient,
            ForgeError::Unsupported(_) => ErrorClass::Unsupported,
            ForgeError::Other(_) => ErrorClass::Other,
        }
    }
    fn unsupported(op: &'static str) -> Self {
        ForgeError::Unsupported(op)
    }
}

// ---------------------------------------------------------------------------
// Caps + small request types
// ---------------------------------------------------------------------------

/// What a forge implementation can do. An optional [`Forge`] op exists iff it
/// has a bit here; the default body returns `Unsupported`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
pub struct ForgeCaps {
    pub pr_status: bool,
    pub pr_list: bool,
    pub pr_search: bool,
    pub create_pr: bool,
    pub merge: bool,
    pub auto_merge: bool,
    pub draft_toggle: bool,
    pub checks_rerun: bool,
    pub reviews: bool,
    pub review_threads: bool,
    /// Atomically reply to and resolve an existing review thread.
    pub resolve_review_thread: bool,
    pub line_comments: bool,
    pub conversation: bool,
    pub pr_diff: bool,
    pub issues: bool,
    pub notifications: bool,
    pub open_in_browser: bool,
    pub whoami: bool,
}

impl ForgeCaps {
    /// Everything (the `gh` CLI).
    pub const ALL: ForgeCaps = ForgeCaps {
        pr_status: true,
        pr_list: true,
        pr_search: true,
        create_pr: true,
        merge: true,
        auto_merge: true,
        draft_toggle: true,
        checks_rerun: true,
        reviews: true,
        review_threads: true,
        resolve_review_thread: true,
        line_comments: true,
        conversation: true,
        pr_diff: true,
        issues: true,
        notifications: true,
        open_in_browser: true,
        whoami: true,
    };
}

/// Which pull request an op targets: the one for the worktree's current
/// branch (the provider infers it), or an explicit number (the PR queue
/// tracks entries with no local checkout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrRef {
    Current,
    Number(u64),
}

impl PrRef {
    pub fn number(self) -> Option<u64> {
        match self {
            PrRef::Current => None,
            PrRef::Number(n) => Some(n),
        }
    }
}

impl From<Option<u64>> for PrRef {
    fn from(n: Option<u64>) -> Self {
        n.map_or(PrRef::Current, PrRef::Number)
    }
}

/// `owner/repo`, parsed once from the PR URL or the origin remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoRef {
    pub owner: String,
    pub repo: String,
}

impl RepoRef {
    pub fn nwo(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// One fetch's result for the PR queue: the PR plus the threads that came
/// with it ("changes requested" is part of classification).
#[derive(Debug, Clone)]
pub struct FetchedPr {
    pub pr: PrStatus,
    pub threads: Vec<ReviewThreadRow>,
}

/// A line comment on a PR diff.
#[derive(Debug, Clone, Copy)]
pub struct LineComment<'a> {
    pub repo: &'a RepoRef,
    pub number: u64,
    pub commit_id: &'a str,
    pub path: &'a str,
    pub line: u64,
    pub body: &'a str,
}

/// A forge-native issue (the `thegn issue` CLI). Distinct from
/// [`crate::issue::Issue`], the tracker-seam shape.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeIssue {
    pub number: u64,
    pub title: String,
    pub body: Option<String>,
    /// `OPEN` | `CLOSED`
    pub state: String,
    pub url: String,
    pub author: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

/// Options for creating a forge issue.
#[derive(Debug, Default, Clone)]
pub struct CreateIssueOpts {
    pub title: String,
    pub body: Option<String>,
    pub labels: Vec<String>,
}

/// A mention notification: `(title, url)`.
pub type Mention = (String, String);

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

macro_rules! unsupported {
    ($op:literal) => {
        Err(ForgeError::Unsupported($op))
    };
}

/// The forge seam. Every method is blocking; call from a blocking thread.
/// `loc` is the worktree the op is about (local, ssh or provider — the
/// implementation routes the command there).
pub trait Forge: Probe + Send + Sync {
    /// Stable id: `"github"`, `"ghe"`, … (the PR queue's `forge` column).
    fn id(&self) -> &'static str;
    fn caps(&self) -> ForgeCaps;

    /// `owner/repo` for a worktree, from its origin remote. Pure string
    /// work on `git remote get-url`; never touches the forge.
    fn repo_ref(&self, loc: &GitLoc) -> Option<RepoRef>;

    // --- pull requests: read --------------------------------------------------

    /// The PR's current state. `NoPr` when there is none.
    fn pr_status(&self, loc: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError>;

    /// The repo's open PRs, one header per branch (the branch-badge feed).
    fn pr_list(&self, loc: &GitLoc, limit: usize) -> Result<Vec<PrHeader>, ForgeError>;

    /// `OPEN`/`MERGED`/`CLOSED` for the PR on `branch`, if any.
    fn pr_state_for_branch(
        &self,
        _loc: &GitLoc,
        _branch: &str,
    ) -> Result<Option<String>, ForgeError> {
        unsupported!("pr_state_for_branch")
    }

    /// PRs involving the caller. `role` is provider-defined (`review-requested`,
    /// `author`); `repo` scopes to one `owner/repo`.
    fn search_prs(
        &self,
        _loc: &GitLoc,
        _role: PrRole,
        _repo: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<PrSearchRow>, ForgeError> {
        unsupported!("search_prs")
    }

    fn review_threads(
        &self,
        _loc: &GitLoc,
        _repo: &RepoRef,
        _number: u64,
    ) -> Result<Vec<ReviewThreadRow>, ForgeError> {
        unsupported!("review_threads")
    }

    fn conversation(
        &self,
        _loc: &GitLoc,
        _repo: &RepoRef,
        _number: u64,
    ) -> Result<PrConversation, ForgeError> {
        unsupported!("conversation")
    }

    /// Raw reviews JSON (the `thegn pr reviews` passthrough).
    fn reviews_json(&self, _loc: &GitLoc) -> Result<String, ForgeError> {
        unsupported!("reviews_json")
    }

    fn pr_diff(&self, _loc: &GitLoc, _pr: PrRef) -> Result<PrDiff, ForgeError> {
        unsupported!("pr_diff")
    }

    /// The raw unified diff text (`thegn pr diff` without `--json`).
    fn pr_diff_raw(&self, _loc: &GitLoc, _pr: PrRef) -> Result<String, ForgeError> {
        unsupported!("pr_diff_raw")
    }

    // --- pull requests: write -------------------------------------------------

    /// Returns the new PR's URL.
    fn create_pr(&self, _loc: &GitLoc, _opts: &CreateOpts) -> Result<String, ForgeError> {
        unsupported!("create_pr")
    }

    /// `auto = true` asks the forge to merge once its own rules allow.
    fn merge_pr(
        &self,
        _loc: &GitLoc,
        _pr: PrRef,
        _method: MergeMethod,
        _delete_branch: bool,
        _auto: bool,
    ) -> Result<(), ForgeError> {
        unsupported!("merge_pr")
    }

    fn set_draft(&self, _loc: &GitLoc, _pr: PrRef, _draft: bool) -> Result<(), ForgeError> {
        unsupported!("set_draft")
    }

    fn set_auto_merge(
        &self,
        _loc: &GitLoc,
        _pr: PrRef,
        _enable: bool,
        _method: MergeMethod,
    ) -> Result<(), ForgeError> {
        unsupported!("set_auto_merge")
    }

    fn comment(&self, _loc: &GitLoc, _pr: PrRef, _body: &str) -> Result<(), ForgeError> {
        unsupported!("comment")
    }

    fn submit_review(
        &self,
        _loc: &GitLoc,
        _pr: PrRef,
        _state: ReviewState,
        _body: Option<&str>,
    ) -> Result<(), ForgeError> {
        unsupported!("submit_review")
    }

    fn reply_thread(&self, _loc: &GitLoc, _thread_id: &str, _body: &str) -> Result<(), ForgeError> {
        unsupported!("reply_thread")
    }

    /// Post a bounded audit reply and resolve the thread as one semantic
    /// provider action. Implementations may use a provider-specific combined
    /// mutation; callers must never sequence vendor operations themselves.
    fn resolve_review_thread(
        &self,
        _loc: &GitLoc,
        _thread_id: &str,
        _bounded_reply: &str,
    ) -> Result<(), ForgeError> {
        unsupported!("resolve_review_thread")
    }

    fn add_line_comment(&self, _loc: &GitLoc, _c: LineComment<'_>) -> Result<(), ForgeError> {
        unsupported!("add_line_comment")
    }

    // --- checks ---------------------------------------------------------------

    /// Re-run the failed checks for the PR's branch; how many were restarted.
    fn rerun_failed(&self, _loc: &GitLoc, _pr: PrRef) -> Result<u32, ForgeError> {
        unsupported!("rerun_failed")
    }

    // --- issues (forge-native) ------------------------------------------------

    /// Open issues as the PR panel's sidebar list.
    fn issue_rows(&self, _loc: &GitLoc, _limit: usize) -> Result<Vec<IssueRow>, ForgeError> {
        unsupported!("issue_rows")
    }
    fn issue_list(&self, _loc: &GitLoc, _state: &str) -> Result<Vec<ForgeIssue>, ForgeError> {
        unsupported!("issue_list")
    }
    fn issue_get(&self, _loc: &GitLoc, _number: u64) -> Result<ForgeIssue, ForgeError> {
        unsupported!("issue_get")
    }
    fn issue_create(
        &self,
        _loc: &GitLoc,
        _opts: &CreateIssueOpts,
    ) -> Result<ForgeIssue, ForgeError> {
        unsupported!("issue_create")
    }
    fn issue_comment(&self, _loc: &GitLoc, _number: u64, _body: &str) -> Result<(), ForgeError> {
        unsupported!("issue_comment")
    }

    // --- misc -----------------------------------------------------------------

    /// Mentions of the caller in `repo` (`(title, url)`), newest first.
    fn mentions(&self, _loc: &GitLoc, _repo: &RepoRef) -> Result<Vec<Mention>, ForgeError> {
        unsupported!("mentions")
    }

    /// Open the PR for `branch` in the browser.
    fn open_in_browser(&self, _loc: &GitLoc, _branch: Option<&str>) -> Result<(), ForgeError> {
        unsupported!("open_in_browser")
    }

    /// The authenticated user's login. The one identity probe: onboarding,
    /// doctor and the PR queue's own-PR check all use it.
    fn whoami(&self, _loc: &GitLoc) -> Result<String, ForgeError> {
        unsupported!("whoami")
    }

    // --- provided compositions -------------------------------------------------

    /// The panel feed: `pr_status` folded into a [`PrPanel`] (every error
    /// becomes a state), plus — at `PrDepth::Full` — best-effort review
    /// threads and the issue list, or at `PrDepth::Threads` threads only.
    /// Extra fetches never fail the panel.
    fn pr_panel(&self, loc: &GitLoc, pr: PrRef, depth: PrDepth) -> PrPanel {
        let branch = loc
            .git_out(&["rev-parse", "--abbrev-ref", "HEAD"])
            .unwrap_or_default();
        let mut panel = PrPanel::from_result(self.pr_status(loc, pr), loc.path(), branch);
        if depth != PrDepth::Header
            && let PanelState::Pr(p) = &panel.state
            && let Some(repo) =
                owner_repo_from_url(&p.url).map(|(owner, repo)| RepoRef { owner, repo })
        {
            panel.threads = self
                .review_threads(loc, &repo, p.number)
                .unwrap_or_default();
        }
        if depth == PrDepth::Full {
            panel.issues = self.issue_rows(loc, 10).unwrap_or_default();
        }
        panel
    }

    /// The PR queue's fetch: status + threads by number, as a `Result`.
    fn fetch_pr(&self, loc: &GitLoc, number: u64) -> Result<FetchedPr, ForgeError> {
        let pr = self.pr_status(loc, PrRef::Number(number))?;
        let threads = owner_repo_from_url(&pr.url)
            .map(|(owner, repo)| RepoRef { owner, repo })
            .and_then(|repo| self.review_threads(loc, &repo, number).ok())
            .unwrap_or_default();
        Ok(FetchedPr { pr, threads })
    }
}

/// How much of the panel feed to fetch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrDepth {
    /// `pr_status` only.
    Header,
    /// + review threads (the queue's classification needs them).
    Threads,
    /// + threads + the issue sidebar (the background cache refresh).
    Full,
}

/// Which relationship `search_prs` filters on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrRole {
    ReviewRequested,
    Author,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seam::ProbeReport;

    struct Bare;
    impl Probe for Bare {
        fn probe(&self) -> ProbeReport {
            ProbeReport::new("forge", "bare", crate::seam::Availability::Ready)
        }
    }
    impl Forge for Bare {
        fn id(&self) -> &'static str {
            "bare"
        }
        fn caps(&self) -> ForgeCaps {
            ForgeCaps {
                pr_status: true,
                pr_list: true,
                ..ForgeCaps::default()
            }
        }
        fn repo_ref(&self, _: &GitLoc) -> Option<RepoRef> {
            None
        }
        fn pr_status(&self, _: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
            match pr {
                PrRef::Number(7) => Ok(PrStatus {
                    number: 7,
                    title: "t".into(),
                    state: "OPEN".into(),
                    url: "https://github.com/acme/widget/pull/7".into(),
                    ..Default::default()
                }),
                PrRef::Number(_) => Err(ForgeError::NoPr),
                PrRef::Current => Err(ForgeError::Offline),
            }
        }
        fn pr_list(&self, _: &GitLoc, _: usize) -> Result<Vec<PrHeader>, ForgeError> {
            Ok(vec![])
        }
    }

    fn loc() -> GitLoc {
        GitLoc::Local(std::env::temp_dir())
    }

    #[test]
    fn optional_ops_default_to_unsupported_and_never_panic() {
        let f = Bare;
        let l = loc();
        let repo = RepoRef {
            owner: "a".into(),
            repo: "b".into(),
        };
        assert_eq!(
            f.pr_diff(&l, PrRef::Current),
            Err(ForgeError::Unsupported("pr_diff"))
        );
        assert_eq!(
            f.merge_pr(&l, PrRef::Current, MergeMethod::Squash, false, false),
            Err(ForgeError::Unsupported("merge_pr"))
        );
        assert!(matches!(
            f.whoami(&l),
            Err(ForgeError::Unsupported("whoami"))
        ));
        assert!(matches!(
            f.review_threads(&l, &repo, 1),
            Err(ForgeError::Unsupported(_))
        ));
        assert!(matches!(
            f.issue_list(&l, "open"),
            Err(ForgeError::Unsupported(_))
        ));
        assert!(matches!(
            f.mentions(&l, &repo),
            Err(ForgeError::Unsupported(_))
        ));
        assert!(matches!(
            f.open_in_browser(&l, None),
            Err(ForgeError::Unsupported(_))
        ));
        assert!(matches!(
            f.add_line_comment(
                &l,
                LineComment {
                    repo: &repo,
                    number: 1,
                    commit_id: "c",
                    path: "p",
                    line: 1,
                    body: "b"
                }
            ),
            Err(ForgeError::Unsupported(_))
        ));
        assert_eq!(
            f.resolve_review_thread(&l, "thread", "reply"),
            Err(ForgeError::Unsupported("resolve_review_thread"))
        );
        // The object-safety contract: a `&dyn Forge` is usable.
        let d: &dyn Forge = &f;
        assert_eq!(d.id(), "bare");
        assert!(d.caps().pr_status && !d.caps().merge);
        assert!(!d.caps().resolve_review_thread);
    }

    #[test]
    fn fetch_pr_composes_status_and_threads() {
        let f = Bare;
        let got = f.fetch_pr(&loc(), 7).unwrap();
        assert_eq!(got.pr.number, 7);
        // review_threads is unsupported → threads empty, fetch still Ok.
        assert!(got.threads.is_empty());
        assert!(matches!(f.fetch_pr(&loc(), 8), Err(ForgeError::NoPr)));
    }

    #[test]
    fn pr_panel_folds_errors_into_states() {
        let f = Bare;
        let p = f.pr_panel(&loc(), PrRef::Current, PrDepth::Full);
        assert!(matches!(p.state, PanelState::Offline));
        assert!(p.threads.is_empty() && p.issues.is_empty());
        let p = f.pr_panel(&loc(), PrRef::Number(7), PrDepth::Threads);
        assert!(matches!(&p.state, PanelState::Pr(pr) if pr.number == 7));
    }

    #[test]
    fn error_classes_and_messages() {
        use crate::seam::SeamError;
        assert_eq!(ForgeError::NotInstalled.class(), ErrorClass::NotInstalled);
        assert_eq!(
            ForgeError::NotConfigured("x").class(),
            ErrorClass::NotConfigured
        );
        assert_eq!(ForgeError::NotAuthenticated.class(), ErrorClass::Auth);
        assert_eq!(ForgeError::NoPr.class(), ErrorClass::NotFound);
        assert_eq!(ForgeError::RateLimited.class(), ErrorClass::RateLimited);
        assert_eq!(ForgeError::Offline.class(), ErrorClass::Transient);
        assert!(ForgeError::Offline.is_transient());
        assert_eq!(
            ForgeError::Unsupported("op").class(),
            ErrorClass::Unsupported
        );
        assert_eq!(ForgeError::Other("m".into()).class(), ErrorClass::Other);
        assert!(ForgeError::NotInstalled.falls_through());
        assert!(!ForgeError::NotAuthenticated.falls_through());
        assert_eq!(ForgeError::unsupported("z"), ForgeError::Unsupported("z"));
        assert_eq!(ForgeError::Other("boom".into()).to_string(), "boom");
        assert!(ForgeError::NoPr.describe().contains("no PR"));
        assert!(
            ForgeError::Unsupported("merge")
                .describe()
                .contains("merge")
        );
        assert!(
            ForgeError::NotConfigured("token")
                .describe()
                .contains("token")
        );
    }

    #[test]
    fn small_types() {
        assert_eq!(PrRef::from(Some(3)), PrRef::Number(3));
        assert_eq!(PrRef::from(None), PrRef::Current);
        assert_eq!(PrRef::Number(3).number(), Some(3));
        assert_eq!(PrRef::Current.number(), None);
        assert_eq!(
            RepoRef {
                owner: "a".into(),
                repo: "b".into()
            }
            .nwo(),
            "a/b"
        );
        let all = ForgeCaps::ALL;
        let none = ForgeCaps::default();
        assert!(all.whoami && all.line_comments && all.resolve_review_thread);
        assert!(!none.pr_status);
        assert_ne!(all, none);
    }
}
