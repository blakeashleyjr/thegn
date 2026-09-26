//! The forge seam's service half: the GitHub ladder (native → CLI), the
//! per-host [`ForgeSet`] the host resolves worktrees through, and probes for
//! `thegn doctor`. The trait, models and the CLI transport live in
//! `thegn_core::forge` / `thegn_core::github`.

pub mod native;

pub use native::GithubNative;
pub use thegn_core::forge::{
    CreateIssueOpts, FetchedPr, Forge, ForgeCaps, ForgeError, ForgeIssue, LineComment, Mention,
    PrDepth, PrRef, PrRole, RepoRef, model,
};

use crate::seam::Ladder;
use thegn_core::config::Config;
use thegn_core::config_forge::ForgeKind;
use thegn_core::forge::model::*;
use thegn_core::github::GithubCli;
use thegn_core::remote::GitLoc;
use thegn_core::seam::{Availability, Kind, Probe, ProbeReport};

// ---------------------------------------------------------------------------
// Ladder forwarding — written once per seam
// ---------------------------------------------------------------------------

macro_rules! forward {
    ($self:ident, $op:literal, |$l:ident| $body:expr) => {
        $self.try_each_sync($op, |$l| $body)
    };
}

impl Probe for Ladder<dyn Forge> {
    fn probe(&self) -> ProbeReport {
        // The ladder is as available as its most basic layer; the notes list
        // every layer's own verdict.
        let reports: Vec<ProbeReport> = self.layers.iter().map(|l| l.probe()).collect();
        let availability = reports
            .last()
            .map(|r| r.availability.clone())
            .unwrap_or(Availability::Unavailable("empty ladder".into()));
        let mut out = ProbeReport::new("forge", self.id, availability).with_caps(&self.caps());
        for r in reports {
            let state = match &r.availability {
                Availability::Ready => "ready".to_string(),
                Availability::Degraded(w) => format!("degraded — {w}"),
                Availability::Unavailable(w) => format!("unavailable — {w}"),
            };
            out = out.note(format!("{}: {state}", r.id));
        }
        out
    }
}

impl Forge for Ladder<dyn Forge> {
    fn id(&self) -> &'static str {
        self.id
    }
    /// The union of the layers' caps: an op is available if any layer has it.
    fn caps(&self) -> ForgeCaps {
        let mut c = ForgeCaps::default();
        for l in &self.layers {
            let lc = l.caps();
            c.pr_status |= lc.pr_status;
            c.pr_list |= lc.pr_list;
            c.pr_search |= lc.pr_search;
            c.create_pr |= lc.create_pr;
            c.merge |= lc.merge;
            c.auto_merge |= lc.auto_merge;
            c.draft_toggle |= lc.draft_toggle;
            c.checks_rerun |= lc.checks_rerun;
            c.reviews |= lc.reviews;
            c.review_threads |= lc.review_threads;
            c.resolve_review_thread |= lc.resolve_review_thread;
            c.line_comments |= lc.line_comments;
            c.conversation |= lc.conversation;
            c.pr_diff |= lc.pr_diff;
            c.issues |= lc.issues;
            c.notifications |= lc.notifications;
            c.open_in_browser |= lc.open_in_browser;
            c.whoami |= lc.whoami;
            c.pr_authorship |= lc.pr_authorship;
        }
        c
    }
    fn repo_ref(&self, loc: &GitLoc) -> Option<RepoRef> {
        self.layers.iter().find_map(|l| l.repo_ref(loc))
    }
    fn pr_status(&self, loc: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
        forward!(self, "pr_status", |l| l.pr_status(loc, pr))
    }
    fn pr_list(&self, loc: &GitLoc, limit: usize) -> Result<Vec<PrHeader>, ForgeError> {
        forward!(self, "pr_list", |l| l.pr_list(loc, limit))
    }
    fn pr_state_for_branch(
        &self,
        loc: &GitLoc,
        branch: &str,
    ) -> Result<Option<String>, ForgeError> {
        forward!(self, "pr_state_for_branch", |l| l
            .pr_state_for_branch(loc, branch))
    }
    fn search_prs(
        &self,
        loc: &GitLoc,
        role: PrRole,
        repo: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PrSearchRow>, ForgeError> {
        forward!(self, "search_prs", |l| l.search_prs(loc, role, repo, limit))
    }
    fn review_threads(
        &self,
        loc: &GitLoc,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ReviewThreadRow>, ForgeError> {
        forward!(self, "review_threads", |l| l
            .review_threads(loc, repo, number))
    }
    fn conversation(
        &self,
        loc: &GitLoc,
        repo: &RepoRef,
        number: u64,
    ) -> Result<PrConversation, ForgeError> {
        forward!(self, "conversation", |l| l.conversation(loc, repo, number))
    }
    fn reviews_json(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        forward!(self, "reviews_json", |l| l.reviews_json(loc))
    }
    fn pr_diff(&self, loc: &GitLoc, pr: PrRef) -> Result<PrDiff, ForgeError> {
        forward!(self, "pr_diff", |l| l.pr_diff(loc, pr))
    }
    fn pr_diff_raw(&self, loc: &GitLoc, pr: PrRef) -> Result<String, ForgeError> {
        forward!(self, "pr_diff_raw", |l| l.pr_diff_raw(loc, pr))
    }
    fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, ForgeError> {
        forward!(self, "create_pr", |l| l.create_pr(loc, opts))
    }
    fn merge_pr(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        method: MergeMethod,
        delete_branch: bool,
        auto: bool,
    ) -> Result<(), ForgeError> {
        forward!(self, "merge_pr", |l| l.merge_pr(
            loc,
            pr,
            method,
            delete_branch,
            auto
        ))
    }
    fn set_draft(&self, loc: &GitLoc, pr: PrRef, draft: bool) -> Result<(), ForgeError> {
        forward!(self, "set_draft", |l| l.set_draft(loc, pr, draft))
    }
    fn set_auto_merge(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        enable: bool,
        method: MergeMethod,
    ) -> Result<(), ForgeError> {
        forward!(self, "set_auto_merge", |l| l
            .set_auto_merge(loc, pr, enable, method))
    }
    fn comment(&self, loc: &GitLoc, pr: PrRef, body: &str) -> Result<(), ForgeError> {
        forward!(self, "comment", |l| l.comment(loc, pr, body))
    }
    fn submit_review(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        state: ReviewState,
        body: Option<&str>,
    ) -> Result<(), ForgeError> {
        forward!(self, "submit_review", |l| l
            .submit_review(loc, pr, state, body))
    }
    fn reply_thread(&self, loc: &GitLoc, thread_id: &str, body: &str) -> Result<(), ForgeError> {
        forward!(self, "reply_thread", |l| l
            .reply_thread(loc, thread_id, body))
    }
    fn resolve_review_thread(
        &self,
        loc: &GitLoc,
        thread_id: &str,
        bounded_reply: &str,
    ) -> Result<(), ForgeError> {
        forward!(self, "resolve_review_thread", |l| l.resolve_review_thread(
            loc,
            thread_id,
            bounded_reply
        ))
    }
    fn add_line_comment(&self, loc: &GitLoc, c: LineComment<'_>) -> Result<(), ForgeError> {
        forward!(self, "add_line_comment", |l| l.add_line_comment(loc, c))
    }
    fn rerun_failed(&self, loc: &GitLoc, pr: PrRef) -> Result<u32, ForgeError> {
        forward!(self, "rerun_failed", |l| l.rerun_failed(loc, pr))
    }
    fn issue_rows(&self, loc: &GitLoc, limit: usize) -> Result<Vec<IssueRow>, ForgeError> {
        forward!(self, "issue_rows", |l| l.issue_rows(loc, limit))
    }
    fn issue_list(&self, loc: &GitLoc, state: &str) -> Result<Vec<ForgeIssue>, ForgeError> {
        forward!(self, "issue_list", |l| l.issue_list(loc, state))
    }
    fn issue_get(&self, loc: &GitLoc, number: u64) -> Result<ForgeIssue, ForgeError> {
        forward!(self, "issue_get", |l| l.issue_get(loc, number))
    }
    fn issue_create(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<ForgeIssue, ForgeError> {
        forward!(self, "issue_create", |l| l.issue_create(loc, opts))
    }
    fn issue_comment(&self, loc: &GitLoc, number: u64, body: &str) -> Result<(), ForgeError> {
        forward!(self, "issue_comment", |l| l
            .issue_comment(loc, number, body))
    }
    fn mentions(&self, loc: &GitLoc, repo: &RepoRef) -> Result<Vec<Mention>, ForgeError> {
        forward!(self, "mentions", |l| l.mentions(loc, repo))
    }
    fn open_in_browser(&self, loc: &GitLoc, branch: Option<&str>) -> Result<(), ForgeError> {
        forward!(self, "open_in_browser", |l| l.open_in_browser(loc, branch))
    }
    fn whoami(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        forward!(self, "whoami", |l| l.whoami(loc))
    }
    fn pr_authorship(
        &self,
        loc: &GitLoc,
        number: u64,
    ) -> Result<thegn_core::forge::authorship::PrAuthorship, ForgeError> {
        forward!(self, "pr_authorship", |l| l.pr_authorship(loc, number))
    }
}

// ---------------------------------------------------------------------------
// Factory + routing
// ---------------------------------------------------------------------------

/// The GitHub ladder: native octocrab reads over the `gh` CLI.
pub fn github(enterprise: bool) -> Ladder<dyn Forge> {
    Ladder::new(
        if enterprise { "ghe" } else { "github" },
        if enterprise {
            // Native currently implements only api.github.com; the CLI resolves
            // enterprise endpoints and host-scoped credentials from the origin.
            vec![Box::new(GithubCli { enterprise }) as Box<dyn Forge>]
        } else {
            vec![
                Box::new(GithubNative::new()) as Box<dyn Forge>,
                Box::new(GithubCli { enterprise }) as Box<dyn Forge>,
            ]
        },
    )
}

/// One forge per `[[forges]]` entry plus the GitHub default. Resolution is by
/// the worktree's `origin` host; with no entries configured every worktree
/// gets the default without spawning git.
pub struct ForgeSet {
    /// `(host, name, forge)` for each configured, non-reserved entry.
    entries: Vec<(String, String, Box<dyn Forge>)>,
    default: Box<dyn Forge>,
}

impl Default for ForgeSet {
    fn default() -> Self {
        ForgeSet {
            entries: Vec::new(),
            default: Box::new(github(false)),
        }
    }
}

impl ForgeSet {
    /// Build from config. Reserved kinds (`forgejo`, `gitea`) produce no
    /// entry — the probe registry reports them; `kind_coverage` pins the rule.
    pub fn from_config(cfg: &Config) -> ForgeSet {
        let mut set = ForgeSet::default();
        for f in &cfg.forges {
            if let Some(forge) = forge_for_kind(f.kind) {
                let host = if f.host.is_empty() {
                    "github.com".to_string()
                } else {
                    f.host.to_ascii_lowercase()
                };
                set.entries.push((host, f.name.clone(), forge));
            }
        }
        set
    }

    /// The forge for a worktree. Sniffs `origin` only when there are
    /// configured entries to route to.
    pub fn for_loc(&self, loc: &GitLoc) -> &dyn Forge {
        if self.entries.is_empty() {
            return self.default.as_ref();
        }
        let host = loc
            .git_out(&["remote", "get-url", "origin"])
            .and_then(|u| remote_host(&u));
        match host {
            Some(h) => self
                .entries
                .iter()
                .find(|(eh, _, _)| *eh == h)
                .map(|(_, _, f)| f.as_ref())
                .unwrap_or(self.default.as_ref()),
            None => self.default.as_ref(),
        }
    }

    /// The default (GitHub) forge — for callers with no worktree in hand.
    pub fn default_forge(&self) -> &dyn Forge {
        self.default.as_ref()
    }

    /// Every forge's probe, for `thegn doctor`.
    pub fn probes(&self) -> Vec<ProbeReport> {
        let mut out = vec![self.default.probe().note("default")];
        for (host, name, f) in &self.entries {
            let mut r = f.probe();
            r.id = format!("{}:{name}", f.id());
            out.push(r.note(format!("host: {host}")));
        }
        out
    }
}

/// The kind → forge factory. `None` for reserved kinds; `kind_coverage`
/// asserts the two agree.
pub fn forge_for_kind(kind: ForgeKind) -> Option<Box<dyn Forge>> {
    if kind.is_reserved() {
        return None;
    }
    match kind {
        ForgeKind::Github => Some(Box::new(github(false))),
        ForgeKind::Ghe => Some(Box::new(github(true))),
        ForgeKind::Forgejo | ForgeKind::Gitea => None,
    }
}

/// Host of a git remote URL, lowercased. Only the authority is inspected, so
/// `@` in a repository path/query/fragment cannot change the route.
pub fn remote_host(url: &str) -> Option<String> {
    if url.is_empty() || url.trim() != url || url.bytes().any(|b| b.is_ascii_control()) {
        return None;
    }
    // A Windows drive path, including drive-relative `C:repo`, is local
    // path syntax, not SCP's `host:path` form.
    if url.len() >= 2 && url.as_bytes()[0].is_ascii_alphabetic() && url.as_bytes()[1] == b':' {
        return None;
    }

    for scheme in ["https://", "http://", "ssh://"] {
        if url
            .get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
        {
            let rest = &url[scheme.len()..];
            let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            return authority_host(&rest[..authority_end]);
        }
    }

    // A supported scheme was not present. Do not reinterpret an unsupported
    // URL scheme such as ftp:// as SCP syntax.
    if url.contains("://") {
        return None;
    }
    scp_host(url)
}

fn authority_host(authority: &str) -> Option<String> {
    if authority.is_empty() || authority.bytes().any(|b| b.is_ascii_whitespace()) {
        return None;
    }
    let host_port = if let Some((userinfo, host_port)) = authority.rsplit_once('@') {
        if !valid_userinfo(userinfo) {
            return None;
        }
        host_port
    } else {
        authority
    };

    if let Some(rest) = host_port.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        host.parse::<std::net::Ipv6Addr>().ok()?;
        // Routing keys are hosts, so discard brackets and any valid port.
        if !suffix.is_empty() {
            parse_port(suffix.strip_prefix(':')?)?;
        }
        return Some(host.to_ascii_lowercase());
    }
    if host_port.contains(']') || host_port.matches(':').count() > 1 {
        return None;
    }
    let host = match host_port.rsplit_once(':') {
        Some((host, suffix)) => {
            parse_port(suffix)?;
            host
        }
        None => host_port,
    };
    valid_dns_host(host).then(|| host.to_ascii_lowercase())
}

fn parse_port(suffix: &str) -> Option<u16> {
    (!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        .then(|| suffix.parse().ok())
        .flatten()
}

fn valid_dns_host(host: &str) -> bool {
    // Forge configuration and routing keys use the existing ASCII hostname
    // policy; Unicode remains valid in the repository path.
    !host.is_empty()
        && host.split('.').all(|part| {
            !part.is_empty()
                && !part.starts_with('-')
                && !part.ends_with('-')
                && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn scp_host(url: &str) -> Option<String> {
    let bracket_open = if url.starts_with('[') {
        Some(0)
    } else {
        url.find("@[").map(|index| index + 1)
    };
    if let Some(open) = bracket_open {
        let prefix = &url[..open];
        if !prefix.is_empty()
            && (!prefix.ends_with('@')
                || prefix[..prefix.len() - 1].is_empty()
                || !valid_userinfo(&prefix[..prefix.len() - 1]))
        {
            return None;
        }
        let rest = &url[open + 1..];
        let (host, suffix) = rest.split_once(']')?;
        host.parse::<std::net::Ipv6Addr>().ok()?;
        suffix.strip_prefix(':')?;
        return Some(host.to_ascii_lowercase());
    }
    let (authority, _) = url.split_once(':')?;
    let authority = if let Some((userinfo, host)) = authority.rsplit_once('@') {
        if !valid_userinfo(userinfo) {
            return None;
        }
        host
    } else {
        authority
    };
    valid_dns_host(authority).then(|| authority.to_ascii_lowercase())
}

fn valid_userinfo(userinfo: &str) -> bool {
    !userinfo.is_empty()
        && !userinfo.bytes().any(|b| {
            matches!(b, b'@' | b'/' | b'?' | b'#' | b'\\')
                || b.is_ascii_whitespace()
                || b.is_ascii_control()
        })
}

/// Every configured forge's probe (the seam registry's view).
pub fn probes(cfg: &Config) -> Vec<ProbeReport> {
    let mut out = ForgeSet::from_config(cfg).probes();
    for f in &cfg.forges {
        if f.kind.is_reserved() {
            out.push(ProbeReport::reserved(
                "forge",
                &format!("{}:{}", f.kind.as_str(), f.name),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Layer {
        id: &'static str,
        status: Result<u64, ForgeError>,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }
    impl Probe for Layer {
        fn probe(&self) -> ProbeReport {
            ProbeReport::new("forge", self.id, Availability::Ready)
        }
    }
    impl Forge for Layer {
        fn id(&self) -> &'static str {
            self.id
        }
        fn caps(&self) -> ForgeCaps {
            ForgeCaps {
                pr_status: true,
                merge: self.id == "cli",
                resolve_review_thread: self.id == "cli",
                pr_authorship: self.id == "cli",
                ..ForgeCaps::default()
            }
        }
        fn repo_ref(&self, _: &GitLoc) -> Option<RepoRef> {
            None
        }
        fn pr_status(&self, _: &GitLoc, _: PrRef) -> Result<PrStatus, ForgeError> {
            self.calls.lock().unwrap().push(self.id);
            self.status.clone().map(|n| PrStatus {
                number: n,
                ..Default::default()
            })
        }
        fn pr_list(&self, _: &GitLoc, _: usize) -> Result<Vec<PrHeader>, ForgeError> {
            Ok(vec![])
        }
        fn pr_authorship(
            &self,
            _: &GitLoc,
            number: u64,
        ) -> Result<thegn_core::forge::authorship::PrAuthorship, ForgeError> {
            self.calls.lock().unwrap().push("pr_authorship");
            self.status.clone()?;
            if self.id != "cli" {
                return Err(ForgeError::Unsupported("pr_authorship"));
            }
            let identity = thegn_core::forge::model::PrAuthor {
                id: self.id.into(),
                login: self.id.into(),
                is_bot: false,
            };
            Ok(thegn_core::forge::authorship::PrAuthorship {
                provider: "github".into(),
                repository: thegn_core::forge::authorship::GithubRepository::from_origin(
                    "https://github.com/org/repo.git",
                )
                .unwrap(),
                repository_id: "R_repo".into(),
                pr_id: "PR_7".into(),
                number,
                head: "a".repeat(40),
                state: "OPEN".into(),
                author: identity.clone(),
                viewer: identity,
            })
        }
        fn merge_pr(
            &self,
            _: &GitLoc,
            _: PrRef,
            _: MergeMethod,
            _: bool,
            _: bool,
        ) -> Result<(), ForgeError> {
            self.calls.lock().unwrap().push("merge");
            Ok(())
        }
        fn resolve_review_thread(&self, _: &GitLoc, _: &str, _: &str) -> Result<(), ForgeError> {
            self.calls.lock().unwrap().push("resolve_review_thread");
            if self.id == "cli" {
                Ok(())
            } else {
                Err(ForgeError::Unsupported("resolve_review_thread"))
            }
        }
    }

    fn ladder(native: Result<u64, ForgeError>) -> Ladder<dyn Forge> {
        Ladder::new(
            "t",
            vec![
                Box::new(Layer {
                    id: "native",
                    status: native,
                    calls: Default::default(),
                }) as Box<dyn Forge>,
                Box::new(Layer {
                    id: "cli",
                    status: Ok(99),
                    calls: Default::default(),
                }),
            ],
        )
    }
    fn loc() -> GitLoc {
        GitLoc::Local(std::env::temp_dir())
    }

    #[test]
    fn not_configured_falls_through_to_cli() {
        let l = ladder(Err(ForgeError::NotConfigured("no token".into())));
        assert_eq!(l.pr_status(&loc(), PrRef::Current).unwrap().number, 99);
        // An op only the CLI layer has: Unsupported on native falls through.
        assert!(
            l.merge_pr(&loc(), PrRef::Current, MergeMethod::Squash, false, false)
                .is_ok()
        );
        assert!(l.resolve_review_thread(&loc(), "thread", "reply").is_ok());
        assert!(l.caps().resolve_review_thread);
    }

    #[test]
    fn authorship_falls_back_as_one_operation_and_auth_errors_remain_final() {
        let l = ladder(Err(ForgeError::NotConfigured("no native identity".into())));
        assert!(l.caps().pr_authorship);
        let proof = l.pr_authorship(&loc(), 7).unwrap();
        assert_eq!(proof.author.id, "cli");
        assert_eq!(proof.viewer.id, "cli");
        assert_eq!(proof.number, 7);
        assert!(matches!(
            ladder(Err(ForgeError::NotAuthenticated)).pr_authorship(&loc(), 7),
            Err(ForgeError::NotAuthenticated)
        ));
        assert!(matches!(
            ladder(Err(ForgeError::Offline)).pr_authorship(&loc(), 7),
            Err(ForgeError::Offline)
        ));
    }

    #[test]
    fn auth_failure_is_final() {
        let l = ladder(Err(ForgeError::NotAuthenticated));
        assert!(matches!(
            l.pr_status(&loc(), PrRef::Current),
            Err(ForgeError::NotAuthenticated)
        ));
        let l = ladder(Err(ForgeError::Offline));
        assert!(matches!(
            l.pr_status(&loc(), PrRef::Current),
            Err(ForgeError::Offline)
        ));
    }

    #[test]
    fn native_answer_wins() {
        let l = ladder(Ok(7));
        assert_eq!(l.pr_status(&loc(), PrRef::Current).unwrap().number, 7);
        // Caps are the union; id is the ladder's.
        assert!(l.caps().merge && l.caps().pr_status && !l.caps().whoami);
        assert_eq!(l.id(), "t");
        let p = l.probe();
        assert_eq!(p.notes.len(), 2);
        assert!(matches!(p.availability, Availability::Ready));
    }

    #[test]
    fn github_ladder_is_native_over_cli_and_kinds_agree_with_reserved() {
        let g = github(false);
        assert_eq!(g.id(), "github");
        assert_eq!(g.layers.len(), 2);
        assert!(g.caps().whoami, "CLI layer supplies the full caps");
        assert_eq!(github(true).id(), "ghe");
        crate::seam::kind_coverage(forge_for_kind);
    }

    #[test]
    fn forge_set_routes_by_host() {
        let mut cfg = Config::default();
        cfg.forges.push(thegn_core::config_forge::ForgeConfig {
            name: "work".into(),
            kind: ForgeKind::Ghe,
            host: "GitHub.Example.com".into(),
            ..Default::default()
        });
        cfg.forges.push(thegn_core::config_forge::ForgeConfig {
            name: "codeberg".into(),
            kind: ForgeKind::Forgejo,
            host: "codeberg.org".into(),
            ..Default::default()
        });
        let set = ForgeSet::from_config(&cfg);
        assert_eq!(set.entries.len(), 1, "reserved kind produces no entry");
        assert_eq!(set.entries[0].0, "github.example.com");
        assert_eq!(set.default_forge().id(), "github");
        // A loc with no origin resolves to the default.
        assert_eq!(set.for_loc(&loc()).id(), "github");
        let probes = probes(&cfg);
        assert!(probes.iter().any(|p| p.id == "ghe:work"));
        assert!(probes.iter().any(|p| p.id == "forgejo:codeberg"
            && matches!(&p.availability, Availability::Unavailable(w) if w.contains("reserved"))));
        // Zero config: default only, no git spawn path.
        assert_eq!(ForgeSet::default().for_loc(&loc()).id(), "github");
    }

    #[test]
    fn remote_host_parses_every_url_shape() {
        for (url, expected) in [
            ("git@github.com:o/r.git", Some("github.com")),
            ("https://GitHub.com/o/r", Some("github.com")),
            (
                "HTTP://git@GitHub.com:443/o/r?next=@evil#frag",
                Some("github.com"),
            ),
            ("ssh://git@codeberg.org:22/o/r", Some("codeberg.org")),
            (
                "ssh://git@evil.example/foo@github.com/bar",
                Some("evil.example"),
            ),
            (
                "https://evil.example/org/repo?next=@github.com",
                Some("evil.example"),
            ),
            (
                "https://evil.example/org/repo#@github.com",
                Some("evil.example"),
            ),
            (
                "https://evil.example/org/é@github.com",
                Some("evil.example"),
            ),
            ("ssh://git@[2001:DB8::1]:22/o/r", Some("2001:db8::1")),
            ("git@[2001:db8::1]:o/r", Some("2001:db8::1")),
        ] {
            assert_eq!(remote_host(url).as_deref(), expected, "{url}");
        }
        for url in [
            "",
            "not a url",
            "github.com/o/r",
            "ftp://github.com/o/r",
            "https:///o/r",
            "https://:443/o/r",
            "https://github.com:not-a-port/o/r",
            "https://github.com:65536/o/r",
            "https://[2001:db8::1/o/r",
            "https://[2001:db8::1]x/o/r",
            "https://é.example/o/r",
            "/tmp/git@github.com:repo",
            "dir/git@github.com:repo",
            "C:/repo",
            r"C:\repo",
            "C:repo",
            "@[2001:db8::1]:repo",
        ] {
            assert_eq!(remote_host(url), None, "{url}");
        }
    }

    #[test]
    fn forge_set_routes_by_actual_authority_from_private_git_origin() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            thegn_core::util::git_cmd(dir.path())
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        assert!(
            thegn_core::util::git_cmd(dir.path())
                .args([
                    "config",
                    "remote.origin.url",
                    "https://evil.example/foo@github.com/bar",
                ])
                .status()
                .unwrap()
                .success()
        );

        let mut cfg = Config::default();
        cfg.forges.push(thegn_core::config_forge::ForgeConfig {
            name: "private".into(),
            kind: ForgeKind::Ghe,
            host: "evil.example".into(),
            ..Default::default()
        });
        let set = ForgeSet::from_config(&cfg);
        assert_eq!(set.for_loc(&GitLoc::Local(dir.path().into())).id(), "ghe");
    }
}
