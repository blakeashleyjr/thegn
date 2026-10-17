use super::native::GhCircuit;
use super::{CreateIssueOpts, Forge, ForgeCaps, ForgeError, ForgeIssue, LineComment, Mention};
use std::sync::Arc;
use thegn_core::forge::authorship::PrAuthorship;
use thegn_core::forge::model::*;
use thegn_core::forge::{PrRef, PrRole, RepoRef};
use thegn_core::remote::GitLoc;
use thegn_core::seam::{Probe, ProbeReport};

pub(crate) struct CliPollBreaker {
    inner: Box<dyn Forge>,
    circuit: Arc<GhCircuit>,
}

impl CliPollBreaker {
    pub(crate) fn new(inner: Box<dyn Forge>, circuit: Arc<GhCircuit>) -> Self {
        Self { inner, circuit }
    }

    fn poll<T>(&self, result: Result<T, ForgeError>) -> Result<T, ForgeError> {
        match result {
            Ok(value) => {
                self.circuit.record_success();
                Ok(value)
            }
            Err(error) => {
                if cli_failure_is_transient(&error) {
                    self.circuit.record_http_failure(error.clone());
                }
                Err(error)
            }
        }
    }

    fn held<T>(&self) -> Option<Result<T, ForgeError>> {
        self.circuit.active_error().map(Err)
    }
}

fn cli_failure_is_transient(error: &ForgeError) -> bool {
    match error {
        ForgeError::RateLimited => true,
        ForgeError::Other(message) => {
            let message = message.to_lowercase();
            let has_http_5xx = message.match_indices("http 5").any(|(index, _)| {
                message
                    .as_bytes()
                    .get(index + "http 5".len()..index + "http 5".len() + 2)
                    .is_some_and(|digits| digits.iter().all(u8::is_ascii_digit))
            });
            has_http_5xx
                || message.contains("bad gateway")
                || message.contains("service unavailable")
                || message.contains("gateway timeout")
                || message.starts_with("parse error")
        }
        _ => false,
    }
}

impl Probe for CliPollBreaker {
    fn probe(&self) -> ProbeReport {
        self.inner.probe()
    }
}

impl Forge for CliPollBreaker {
    fn id(&self) -> &'static str {
        self.inner.id()
    }
    fn caps(&self) -> ForgeCaps {
        self.inner.caps()
    }
    fn repo_ref(&self, loc: &GitLoc) -> Option<RepoRef> {
        self.inner.repo_ref(loc)
    }
    fn pr_status(&self, loc: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
        if let Some(held) = self.held() {
            return held;
        }
        self.poll(self.inner.pr_status(loc, pr))
    }
    fn pr_list(&self, loc: &GitLoc, limit: usize) -> Result<Vec<PrHeader>, ForgeError> {
        if let Some(held) = self.held() {
            return held;
        }
        self.poll(self.inner.pr_list(loc, limit))
    }
    fn pr_state_for_branch(
        &self,
        loc: &GitLoc,
        branch: &str,
    ) -> Result<Option<String>, ForgeError> {
        self.inner.pr_state_for_branch(loc, branch)
    }
    fn search_prs(
        &self,
        loc: &GitLoc,
        role: PrRole,
        repo: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PrSearchRow>, ForgeError> {
        if let Some(held) = self.held() {
            return held;
        }
        self.poll(self.inner.search_prs(loc, role, repo, limit))
    }
    fn review_threads(
        &self,
        loc: &GitLoc,
        repo: &RepoRef,
        number: u64,
    ) -> Result<Vec<ReviewThreadRow>, ForgeError> {
        self.inner.review_threads(loc, repo, number)
    }
    fn conversation(
        &self,
        loc: &GitLoc,
        repo: &RepoRef,
        number: u64,
    ) -> Result<PrConversation, ForgeError> {
        self.inner.conversation(loc, repo, number)
    }
    fn reviews_json(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        self.inner.reviews_json(loc)
    }
    fn pr_diff(&self, loc: &GitLoc, pr: PrRef) -> Result<PrDiff, ForgeError> {
        self.inner.pr_diff(loc, pr)
    }
    fn pr_diff_raw(&self, loc: &GitLoc, pr: PrRef) -> Result<String, ForgeError> {
        self.inner.pr_diff_raw(loc, pr)
    }
    fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, ForgeError> {
        self.inner.create_pr(loc, opts)
    }
    fn merge_pr(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        method: MergeMethod,
        delete_branch: bool,
        expected_head: Option<&str>,
        auto: bool,
    ) -> Result<(), ForgeError> {
        self.inner
            .merge_pr(loc, pr, method, delete_branch, expected_head, auto)
    }
    fn set_draft(&self, loc: &GitLoc, pr: PrRef, draft: bool) -> Result<(), ForgeError> {
        self.inner.set_draft(loc, pr, draft)
    }
    fn set_auto_merge(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        enable: bool,
        method: MergeMethod,
    ) -> Result<(), ForgeError> {
        self.inner.set_auto_merge(loc, pr, enable, method)
    }
    fn comment(&self, loc: &GitLoc, pr: PrRef, body: &str) -> Result<(), ForgeError> {
        self.inner.comment(loc, pr, body)
    }
    fn submit_review(
        &self,
        loc: &GitLoc,
        pr: PrRef,
        state: ReviewState,
        body: Option<&str>,
    ) -> Result<(), ForgeError> {
        self.inner.submit_review(loc, pr, state, body)
    }
    fn reply_thread(&self, loc: &GitLoc, thread_id: &str, body: &str) -> Result<(), ForgeError> {
        self.inner.reply_thread(loc, thread_id, body)
    }
    fn resolve_review_thread(
        &self,
        loc: &GitLoc,
        thread_id: &str,
        bounded_reply: &str,
    ) -> Result<(), ForgeError> {
        self.inner
            .resolve_review_thread(loc, thread_id, bounded_reply)
    }
    fn add_line_comment(&self, loc: &GitLoc, comment: LineComment<'_>) -> Result<(), ForgeError> {
        self.inner.add_line_comment(loc, comment)
    }
    fn rerun_failed(&self, loc: &GitLoc, pr: PrRef) -> Result<u32, ForgeError> {
        self.inner.rerun_failed(loc, pr)
    }
    fn issue_rows(&self, loc: &GitLoc, limit: usize) -> Result<Vec<IssueRow>, ForgeError> {
        if let Some(held) = self.held() {
            return held;
        }
        self.poll(self.inner.issue_rows(loc, limit))
    }
    fn issue_list(&self, loc: &GitLoc, state: &str) -> Result<Vec<ForgeIssue>, ForgeError> {
        self.inner.issue_list(loc, state)
    }
    fn issue_get(&self, loc: &GitLoc, number: u64) -> Result<ForgeIssue, ForgeError> {
        self.inner.issue_get(loc, number)
    }
    fn issue_create(&self, loc: &GitLoc, opts: &CreateIssueOpts) -> Result<ForgeIssue, ForgeError> {
        self.inner.issue_create(loc, opts)
    }
    fn issue_comment(&self, loc: &GitLoc, number: u64, body: &str) -> Result<(), ForgeError> {
        self.inner.issue_comment(loc, number, body)
    }
    fn mentions(&self, loc: &GitLoc, repo: &RepoRef) -> Result<Vec<Mention>, ForgeError> {
        self.inner.mentions(loc, repo)
    }
    fn open_in_browser(&self, loc: &GitLoc, branch: Option<&str>) -> Result<(), ForgeError> {
        self.inner.open_in_browser(loc, branch)
    }
    fn whoami(&self, loc: &GitLoc) -> Result<String, ForgeError> {
        self.inner.whoami(loc)
    }
    fn pr_authorship(&self, loc: &GitLoc, number: u64) -> Result<PrAuthorship, ForgeError> {
        self.inner.pr_authorship(loc, number)
    }

    // pr_panel and fetch_pr deliberately use their default compositions so their component polls pass through this breaker.
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use thegn_core::seam::{Availability, ProbeReport};

    struct CountingForge {
        result: Mutex<Option<ForgeError>>,
        polls: AtomicUsize,
        mutations: AtomicUsize,
    }

    impl CountingForge {
        fn new(result: Option<ForgeError>) -> Self {
            Self {
                result: Mutex::new(result),
                polls: AtomicUsize::new(0),
                mutations: AtomicUsize::new(0),
            }
        }

        fn outcome(&self) -> Result<(), ForgeError> {
            self.polls.fetch_add(1, Ordering::Relaxed);
            match self
                .result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
            {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    impl Probe for CountingForge {
        fn probe(&self) -> ProbeReport {
            ProbeReport::new("forge", "counting", Availability::Ready)
        }
    }

    impl Forge for CountingForge {
        fn id(&self) -> &'static str {
            "counting"
        }
        fn caps(&self) -> ForgeCaps {
            ForgeCaps::ALL
        }
        fn repo_ref(&self, _: &GitLoc) -> Option<RepoRef> {
            Some(RepoRef {
                owner: "acme".into(),
                repo: "repo".into(),
            })
        }
        fn pr_status(&self, _: &GitLoc, _: PrRef) -> Result<PrStatus, ForgeError> {
            self.outcome().map(|()| PrStatus::default())
        }
        fn pr_list(&self, _: &GitLoc, _: usize) -> Result<Vec<PrHeader>, ForgeError> {
            self.outcome().map(|()| Vec::new())
        }
        fn search_prs(
            &self,
            _: &GitLoc,
            _: PrRole,
            _: Option<&str>,
            _: usize,
        ) -> Result<Vec<PrSearchRow>, ForgeError> {
            self.outcome().map(|()| Vec::new())
        }
        fn issue_rows(&self, _: &GitLoc, _: usize) -> Result<Vec<IssueRow>, ForgeError> {
            self.outcome().map(|()| Vec::new())
        }
        fn merge_pr(
            &self,
            _: &GitLoc,
            _: PrRef,
            _: MergeMethod,
            _: bool,
            _: Option<&str>,
            _: bool,
        ) -> Result<(), ForgeError> {
            self.mutations.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn create_pr(&self, _: &GitLoc, _: &CreateOpts) -> Result<String, ForgeError> {
            self.mutations.fetch_add(1, Ordering::Relaxed);
            Ok("https://example.test/pr/1".into())
        }
    }

    // Shared adapter keeps test counters observable through the trait object.
    struct SharedCountingForge(Arc<CountingForge>);
    impl Probe for SharedCountingForge {
        fn probe(&self) -> ProbeReport {
            self.0.probe()
        }
    }
    impl Forge for SharedCountingForge {
        fn id(&self) -> &'static str {
            self.0.id()
        }
        fn caps(&self) -> ForgeCaps {
            self.0.caps()
        }
        fn repo_ref(&self, loc: &GitLoc) -> Option<RepoRef> {
            self.0.repo_ref(loc)
        }
        fn pr_status(&self, loc: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
            self.0.pr_status(loc, pr)
        }
        fn pr_list(&self, loc: &GitLoc, limit: usize) -> Result<Vec<PrHeader>, ForgeError> {
            self.0.pr_list(loc, limit)
        }
        fn search_prs(
            &self,
            loc: &GitLoc,
            role: PrRole,
            repo: Option<&str>,
            limit: usize,
        ) -> Result<Vec<PrSearchRow>, ForgeError> {
            self.0.search_prs(loc, role, repo, limit)
        }
        fn issue_rows(&self, loc: &GitLoc, limit: usize) -> Result<Vec<IssueRow>, ForgeError> {
            self.0.issue_rows(loc, limit)
        }
        fn merge_pr(
            &self,
            loc: &GitLoc,
            pr: PrRef,
            method: MergeMethod,
            delete_branch: bool,
            expected_head: Option<&str>,
            auto: bool,
        ) -> Result<(), ForgeError> {
            self.0
                .merge_pr(loc, pr, method, delete_branch, expected_head, auto)
        }
        fn create_pr(&self, loc: &GitLoc, opts: &CreateOpts) -> Result<String, ForgeError> {
            self.0.create_pr(loc, opts)
        }
    }
    #[test]
    fn cli_failures_open_hold_and_success_resets_the_shared_circuit() {
        let circuit = super::super::native::cli_poll_circuit_for_test("cli-pr-status-regression");
        circuit.record_success();
        let inner = Arc::new(CountingForge::new(Some(ForgeError::Other(
            "http 502: bad gateway".into(),
        ))));
        let breaker = CliPollBreaker::new(
            Box::new(SharedCountingForge(Arc::clone(&inner))),
            Arc::clone(&circuit),
        );
        let loc = GitLoc::Local(std::env::temp_dir());
        for _ in 0..3 {
            assert!(matches!(
                breaker.pr_status(&loc, PrRef::Current),
                Err(ForgeError::Other(_))
            ));
        }
        let opened = circuit.active_error().is_some();
        let count = inner.polls.load(Ordering::Relaxed);
        let held_errors = (0..10)
            .map(|_| breaker.pr_status(&loc, PrRef::Current).is_err())
            .all(|held| held);
        let held_count = inner.polls.load(Ordering::Relaxed);
        let held_failures = circuit.failure_count();
        circuit.record_success();
        *inner
            .result
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let success_reached_inner = breaker.pr_status(&loc, PrRef::Current).is_ok();
        let reset_failures = circuit.failure_count();
        circuit.record_success();
        assert!(opened);
        assert!(held_errors);
        assert_eq!(held_count, count);
        assert_eq!(held_failures, 3);
        assert!(success_reached_inner);
        assert_eq!(reset_failures, 0);
    }

    #[test]
    fn rate_limit_gates_search_but_mutations_pass_through_and_contracts_forward() {
        let circuit = super::super::native::cli_poll_circuit_for_test("cli-search-regression");
        circuit.record_success();
        let inner = Arc::new(CountingForge::new(Some(ForgeError::RateLimited)));
        let breaker = CliPollBreaker::new(
            Box::new(SharedCountingForge(Arc::clone(&inner))),
            Arc::clone(&circuit),
        );
        let loc = GitLoc::Local(std::env::temp_dir());
        let search_results: Vec<_> = (0..4)
            .map(|_| breaker.search_prs(&loc, PrRole::Author, None, 5))
            .collect();
        let search_calls = inner.polls.load(Ordering::Relaxed);
        let merge_result = breaker.merge_pr(
            &loc,
            PrRef::Current,
            MergeMethod::Squash,
            false,
            None,
            false,
        );
        let create_result = breaker.create_pr(
            &loc,
            &CreateOpts {
                title: None,
                body: None,
                base: None,
                draft: false,
                web: false,
                fill: false,
            },
        );
        let mutation_calls = inner.mutations.load(Ordering::Relaxed);
        let contracts_match = breaker.id() == inner.id()
            && breaker.caps() == inner.caps()
            && breaker.repo_ref(&loc) == inner.repo_ref(&loc);
        let poll_gate_observes_cli_circuit = super::super::native::poll_backoff_error().is_some();
        circuit.record_success();
        assert!(
            search_results[..3]
                .iter()
                .all(|result| matches!(result, Err(ForgeError::RateLimited)))
        );
        assert!(matches!(
            search_results.get(3),
            Some(Err(ForgeError::RateLimited))
        ));
        assert_eq!(search_calls, 3);
        assert_eq!(merge_result, Ok(()));
        assert!(create_result.is_ok());
        assert_eq!(mutation_calls, 2);
        assert!(contracts_match);
        assert!(poll_gate_observes_cli_circuit);
    }

    #[test]
    fn non_transient_cli_failures_do_not_open_circuit() {
        for (index, error) in [
            ForgeError::Offline,
            ForgeError::NotAuthenticated,
            ForgeError::NoPr,
        ]
        .into_iter()
        .enumerate()
        {
            let circuit = super::super::native::cli_poll_circuit_for_test(&format!(
                "cli-nontransient-{index}"
            ));
            circuit.record_success();
            let inner = Arc::new(CountingForge::new(Some(error.clone())));
            let breaker = CliPollBreaker::new(
                Box::new(SharedCountingForge(Arc::clone(&inner))),
                Arc::clone(&circuit),
            );
            let loc = GitLoc::Local(std::env::temp_dir());
            for _ in 0..5 {
                let result = breaker.issue_rows(&loc, 10);
                assert!(matches!(result, Err(ref got) if got == &error));
            }
            let calls = inner.polls.load(Ordering::Relaxed);
            let stayed_closed = circuit.active_error().is_none();
            circuit.record_success();
            assert_eq!(calls, 5);
            assert!(stayed_closed);
        }
    }

    #[test]
    fn transient_classifier_matches_only_documented_cli_failures() {
        for message in [
            "http 500: upstream",
            "http 502: bad gateway",
            "bad gateway",
            "service unavailable",
            "gateway timeout",
            "parse error: malformed json",
        ] {
            assert!(
                cli_failure_is_transient(&ForgeError::Other(message.into())),
                "{message}"
            );
        }
        for message in [
            "http 404: missing",
            "http 499: client",
            "parse error suffix",
            "other",
        ] {
            assert!(
                !cli_failure_is_transient(&ForgeError::Other(message.into())),
                "{message}"
            );
        }
        assert!(cli_failure_is_transient(&ForgeError::RateLimited));
        for error in [
            ForgeError::Offline,
            ForgeError::NotAuthenticated,
            ForgeError::NoPr,
            ForgeError::NotInstalled,
            ForgeError::NotConfigured("x".into()),
            ForgeError::Unsupported("x"),
        ] {
            assert!(!cli_failure_is_transient(&error));
        }
    }
}
