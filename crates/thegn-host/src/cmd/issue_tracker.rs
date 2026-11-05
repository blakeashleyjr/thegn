//! `thegn issue tracker get|update|comment` (THE-324 slice e): exact-account tracker issue verbs.

use thegn_core::issue::{Issue, IssueDetail, IssuePatch, IssuePriority, IssueRef, IssueStatus};
use thegn_core::{msg, outln};
use thegn_svc::issue::{IssueError, IssueRouter};

#[derive(clap::Subcommand, Clone)]
pub enum TrackerAction {
    /// Show a tracker issue with its comments (exact account).
    Get {
        /// Tracker issue id as `issue list --json` prints it: `<provider>:<key>`.
        id: String,
        /// The configured `[[issues.issue_accounts]]` name that owns the issue.
        #[arg(long)]
        account: String,
        #[arg(long)]
        json: bool,
    },
    /// Patch a tracker issue (exact account).
    Update {
        id: String,
        #[arg(long)]
        account: String,
        #[arg(long, value_parser = ["backlog", "todo", "in_progress", "done", "cancelled"])]
        status: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, value_parser = ["urgent", "high", "medium", "low", "none"])]
        priority: Option<String>,
        /// Assign the issue to the authenticated user.
        #[arg(long)]
        assign_me: bool,
        /// Unassign the authenticated user.
        #[arg(long, conflicts_with = "assign_me")]
        unassign_me: bool,
        #[arg(long)]
        json: bool,
    },
    /// Add a comment to a tracker issue (exact account).
    Comment {
        id: String,
        #[arg(long)]
        account: String,
        body: String,
    },
}

/// What one invocation asks of the router (built purely, before any I/O).
pub(crate) enum Op {
    Get(IssueRef),
    Update(IssueRef, IssuePatch),
    Comment(IssueRef, String),
}

/// What the router answered.
pub(crate) enum Outcome {
    Detail(IssueDetail),
    Updated(Issue),
    Commented(IssueRef),
}

/// `<provider>:<key>` + account -> a typed ref.
pub(crate) fn tracker_ref(id: &str, account: &str) -> std::result::Result<IssueRef, String> {
    let shown = id.chars().take(256).collect::<String>();
    if id.starts_with("plugin:") {
        return Err(format!(
            "plugin issue providers are served only by the running thegn UI: {shown}"
        ));
    }
    let Some((provider, _)) = id.split_once(':') else {
        return Err(format!(
            "an account-qualified issue id must be `<provider>:<key>`: {shown}"
        ));
    };
    IssueRef::new(provider, account, id).map_err(|error| format!("{error} for {shown}"))
}

/// Flags -> `IssuePatch`.
pub(crate) fn build_patch(
    status: Option<&str>,
    title: Option<String>,
    priority: Option<&str>,
    assign_me: bool,
    unassign_me: bool,
) -> std::result::Result<IssuePatch, String> {
    if assign_me && unassign_me {
        return Err("--assign-me and --unassign-me conflict".into());
    }
    if title
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err("--title may not be empty".into());
    }
    let status = status
        .map(|value| match value {
            "backlog" => Ok(IssueStatus::Backlog),
            "todo" => Ok(IssueStatus::Todo),
            "in_progress" => Ok(IssueStatus::InProgress),
            "done" => Ok(IssueStatus::Done),
            "cancelled" => Ok(IssueStatus::Cancelled),
            _ => Err(format!("unknown issue status: {value}")),
        })
        .transpose()?;
    let priority = priority
        .map(|value| match value {
            "urgent" => Ok(IssuePriority::Urgent),
            "high" => Ok(IssuePriority::High),
            "medium" => Ok(IssuePriority::Medium),
            "low" => Ok(IssuePriority::Low),
            "none" => Ok(IssuePriority::None),
            _ => Err(format!("unknown issue priority: {value}")),
        })
        .transpose()?;
    let assignee_me = if assign_me {
        Some(true)
    } else if unassign_me {
        Some(false)
    } else {
        None
    };
    if status.is_none() && title.is_none() && priority.is_none() && assignee_me.is_none() {
        return Err(
            "nothing to update: pass --status, --title, --priority, --assign-me or --unassign-me"
                .into(),
        );
    }
    Ok(IssuePatch {
        status,
        title,
        assignee_me,
        priority,
    })
}

pub(crate) async fn execute(
    router: &IssueRouter,
    op: Op,
) -> std::result::Result<Outcome, IssueError> {
    match op {
        Op::Get(issue_ref) => router.get_issue_ref(&issue_ref).await.map(Outcome::Detail),
        Op::Update(issue_ref, patch) => router
            .update_issue_ref(&issue_ref, &patch)
            .await
            .map(Outcome::Updated),
        Op::Comment(issue_ref, body) => router
            .add_comment_ref(&issue_ref, &body)
            .await
            .map(|()| Outcome::Commented(issue_ref)),
    }
}

/// Render provider data only after making every supplied string terminal-safe.
pub(crate) fn render_text(outcome: &Outcome) -> Vec<String> {
    let safe = |value: &str| msg::terminal_safe(value).into_owned();
    match outcome {
        Outcome::Detail(detail) => {
            let issue = &detail.issue;
            let mut lines = vec![format!(
                "{} {} {}",
                issue.status.glyph(),
                safe(&issue.number),
                safe(&issue.title)
            )];
            lines.push(safe(&issue.url));
            lines.push(format!(
                "Status: {}  Priority: {}  Account: {}",
                issue.status.label(),
                issue.priority.label(),
                issue
                    .account
                    .as_deref()
                    .map(safe)
                    .unwrap_or_else(|| "-".into())
            ));
            if !issue.labels.is_empty() {
                lines.push(format!(
                    "Labels: {}",
                    issue
                        .labels
                        .iter()
                        .map(|label| safe(label))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            if let Some(body) = issue.body.as_deref().filter(|body| !body.is_empty()) {
                lines.push(String::new());
                lines.push(safe(body));
            }
            for comment in &detail.comments {
                lines.push(String::new());
                lines.push(format!(
                    "{} (unix ms {}):",
                    safe(&comment.author),
                    comment.created_at_ms
                ));
                lines.push(safe(&comment.body));
            }
            if !detail.comments_complete {
                lines.push("(comments incomplete: the provider returned a partial page)".into());
            }
            lines
        }
        Outcome::Updated(issue) => vec![format!(
            "Updated {} {} [{}]",
            safe(&issue.number),
            safe(&issue.title),
            issue.status.as_str()
        )],
        Outcome::Commented(issue_ref) => vec![format!(
            "Comment added to {} ({})",
            safe(&issue_ref.id),
            safe(&issue_ref.account)
        )],
    }
}

pub fn run(
    cfg: &thegn_core::config::Config,
    action: TrackerAction,
    config_path: &std::path::Path,
) -> anyhow::Result<()> {
    let (op, verb, json) = match action {
        TrackerAction::Get { id, account, json } => (
            Op::Get(
                tracker_ref(&id, &account)
                    .unwrap_or_else(|error| msg::die(&format!("issue tracker: {error}"))),
            ),
            "get",
            json,
        ),
        TrackerAction::Update {
            id,
            account,
            status,
            title,
            priority,
            assign_me,
            unassign_me,
            json,
        } => (
            Op::Update(
                tracker_ref(&id, &account)
                    .unwrap_or_else(|error| msg::die(&format!("issue tracker: {error}"))),
                build_patch(
                    status.as_deref(),
                    title,
                    priority.as_deref(),
                    assign_me,
                    unassign_me,
                )
                .unwrap_or_else(|error| msg::die(&format!("issue tracker: {error}"))),
            ),
            "update",
            json,
        ),
        TrackerAction::Comment { id, account, body } => {
            if body.trim().is_empty() {
                msg::die("issue tracker: the comment body may not be empty");
            }
            (
                Op::Comment(
                    tracker_ref(&id, &account)
                        .unwrap_or_else(|error| msg::die(&format!("issue tracker: {error}"))),
                    body,
                ),
                "comment",
                false,
            )
        }
    };
    let router = super::issue::tracker_router(cfg, std::env::current_dir().ok().as_deref());
    if !router.is_configured() {
        msg::die(&super::issue::no_tracker_message(
            &super::issue::skipped_plugin_issue_providers(
                cfg,
                config_path.parent().unwrap_or(std::path::Path::new("")),
            ),
        ));
    }
    let runtime = tokio::runtime::Runtime::new()?;
    match runtime.block_on(execute(&router, op)) {
        Ok(outcome) => match (&outcome, json) {
            (Outcome::Detail(detail), true) => super::emit_json(detail),
            (Outcome::Updated(issue), true) => super::emit_json(issue),
            _ => {
                for line in render_text(&outcome) {
                    outln!("{line}");
                }
                Ok(())
            }
        },
        Err(error) => msg::die(&format!("issue tracker {verb} failed: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use thegn_core::config::IssuesConfig;
    use thegn_core::issue::{ISSUE_REF_VERSION, IssueComment};
    use thegn_svc::issue::{IssueBackend, IssueCaps};

    type BackendResult<T> = std::result::Result<T, IssueError>;

    #[derive(clap::Parser)]
    struct Wrapper {
        #[command(subcommand)]
        action: TrackerAction,
    }

    struct RecordingBackend {
        account: &'static str,
        calls: std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl IssueBackend for RecordingBackend {
        fn provider_id(&self) -> &str {
            "plugin:demo"
        }

        fn caps(&self) -> IssueCaps {
            IssueCaps {
                patch_status: true,
                patch_assignee_me: true,
                ..Default::default()
            }
        }

        fn list_issues<'a>(
            &'a self,
            _filter: &'a thegn_core::issue::IssueFilter,
        ) -> futures::future::BoxFuture<'a, BackendResult<Vec<Issue>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn get_issue<'a>(
            &'a self,
            _id: &'a str,
        ) -> futures::future::BoxFuture<'a, BackendResult<IssueDetail>> {
            Box::pin(async { Err(IssueError::Api("unused".into())) })
        }

        fn create_issue<'a>(
            &'a self,
            _draft: &'a thegn_core::issue::IssueDraft,
        ) -> futures::future::BoxFuture<'a, BackendResult<Issue>> {
            Box::pin(async { Err(IssueError::Api("unused".into())) })
        }

        fn update_issue<'a>(
            &'a self,
            id: &'a str,
            _patch: &'a IssuePatch,
        ) -> futures::future::BoxFuture<'a, BackendResult<Issue>> {
            let account = self.account;
            let calls = self.calls.clone();
            Box::pin(async move {
                calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(account);
                Ok(Issue {
                    id: id.to_owned(),
                    provider: "plugin:demo".into(),
                    number: "1".into(),
                    ..Default::default()
                })
            })
        }

        fn search<'a>(
            &'a self,
            _query: &'a str,
            _limit: usize,
        ) -> futures::future::BoxFuture<'a, BackendResult<Vec<Issue>>> {
            Box::pin(async { Ok(Vec::new()) })
        }

        fn add_comment<'a>(
            &'a self,
            _id: &'a str,
            _body: &'a str,
        ) -> futures::future::BoxFuture<'a, BackendResult<()>> {
            let account = self.account;
            let calls = self.calls.clone();
            Box::pin(async move {
                calls
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(account);
                Ok(())
            })
        }
    }

    fn recording_router(
        accounts: &[&'static str],
        calls: &std::sync::Arc<std::sync::Mutex<Vec<&'static str>>>,
    ) -> IssueRouter {
        let mut router = IssueRouter::from_config(&IssuesConfig::default());
        for account in accounts {
            router
                .push_backend(
                    (*account).into(),
                    Box::new(RecordingBackend {
                        account,
                        calls: calls.clone(),
                    }),
                )
                .unwrap();
        }
        router
    }

    fn run_op(router: &IssueRouter, op: Op) -> BackendResult<Outcome> {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(execute(router, op))
    }

    fn issue_ref(account: &str) -> IssueRef {
        IssueRef::new("plugin:demo", account, "plugin:demo:1").unwrap()
    }

    #[test]
    fn tracker_ref_splits_the_provider_from_the_id_and_binds_the_account() {
        let reference = tracker_ref("linear:THE-76", "work").unwrap();
        assert_eq!(reference.provider, "linear");
        assert_eq!(reference.account, "work");
        assert_eq!(reference.id, "linear:THE-76");
        assert_eq!(reference.v, ISSUE_REF_VERSION);
    }

    #[test]
    fn tracker_ref_refuses_an_unqualified_id() {
        assert!(
            tracker_ref("THE-76", "work")
                .unwrap_err()
                .contains("<provider>:<key>")
        );
    }

    #[test]
    fn tracker_ref_refuses_a_plugin_provider_id() {
        assert!(
            tracker_ref("plugin:demo:1", "home")
                .unwrap_err()
                .contains("only by the running thegn UI")
        );
        let long_id = "x".repeat(300);
        assert!(tracker_ref(&long_id, "work").unwrap_err().len() < 400);
    }

    #[test]
    fn build_patch_maps_every_flag() {
        let patch = build_patch(
            Some("in_progress"),
            Some("New".into()),
            Some("urgent"),
            true,
            false,
        )
        .unwrap();
        assert_eq!(patch.status, Some(IssueStatus::InProgress));
        assert_eq!(patch.title.as_deref(), Some("New"));
        assert_eq!(patch.priority, Some(IssuePriority::Urgent));
        assert_eq!(patch.assignee_me, Some(true));
        let patch = build_patch(None, None, None, false, true).unwrap();
        assert_eq!(patch.assignee_me, Some(false));
    }

    #[test]
    fn build_patch_refuses_an_empty_patch_and_conflicting_assignee_flags() {
        assert!(
            build_patch(None, None, None, false, false)
                .unwrap_err()
                .contains("nothing to update")
        );
        assert!(
            build_patch(None, None, None, true, true)
                .unwrap_err()
                .contains("conflict")
        );
        assert!(
            build_patch(None, Some("  ".into()), None, false, false)
                .unwrap_err()
                .contains("may not be empty")
        );
        assert!(build_patch(Some("wip"), None, None, false, false).is_err());
    }

    #[test]
    fn tracker_verbs_parse_and_require_an_account() {
        let parsed =
            Wrapper::try_parse_from(["w", "get", "linear:THE-1", "--account", "work"]).unwrap();
        assert!(matches!(parsed.action, TrackerAction::Get { .. }));
        assert!(Wrapper::try_parse_from(["w", "get", "linear:THE-1"]).is_err());
        assert!(
            Wrapper::try_parse_from([
                "w",
                "update",
                "linear:THE-1",
                "--account",
                "work",
                "--assign-me",
                "--unassign-me"
            ])
            .is_err()
        );
        assert!(
            Wrapper::try_parse_from([
                "w",
                "update",
                "linear:THE-1",
                "--account",
                "work",
                "--status",
                "wip"
            ])
            .is_err()
        );
        assert!(
            Wrapper::try_parse_from(["w", "comment", "linear:THE-1", "--account", "work", "hi"])
                .is_ok()
        );
    }

    #[test]
    fn get_with_an_unknown_account_is_refused_before_any_backend_call() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = recording_router(&["work"], &calls);
        let result = run_op(&router, Op::Get(issue_ref("other")));
        assert!(matches!(result, Err(IssueError::UnknownAccount { .. })));
        assert!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn unknown_account_is_refused_for_update_and_comment_too() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = recording_router(&["work"], &calls);
        let patch = IssuePatch {
            status: Some(IssueStatus::Done),
            ..Default::default()
        };
        assert!(matches!(
            run_op(&router, Op::Update(issue_ref("other"), patch)),
            Err(IssueError::UnknownAccount { .. })
        ));
        assert!(matches!(
            run_op(&router, Op::Comment(issue_ref("other"), "hi".into())),
            Err(IssueError::UnknownAccount { .. })
        ));
        assert!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty()
        );
    }

    #[test]
    fn comment_reaches_only_the_named_account() {
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let router = recording_router(&["work", "home"], &calls);
        let result = run_op(&router, Op::Comment(issue_ref("home"), "hello".into())).unwrap();
        assert!(matches!(result, Outcome::Commented(reference) if reference.account == "home"));
        assert_eq!(
            calls
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &["home"]
        );
    }

    #[test]
    fn render_text_sanitises_provider_text_and_flags_incomplete_comments() {
        let detail = IssueDetail {
            issue: Issue {
                id: "linear:THE-1".into(),
                number: "THE-1".into(),
                provider: "linear".into(),
                title: "\x1b[2Jhi".into(),
                body: Some("issue body".into()),
                status: IssueStatus::Todo,
                priority: IssuePriority::High,
                labels: vec!["bug".into()],
                url: "https://example.test/issue".into(),
                account: Some("work".into()),
                ..Default::default()
            },
            comments: vec![IssueComment {
                author: "author".into(),
                body: "comment\u{202e}".into(),
                created_at_ms: 7,
            }],
            comments_complete: false,
        };
        let lines = render_text(&Outcome::Detail(detail));
        assert!(
            lines
                .iter()
                .all(|line| !line.contains('\x1b') && !line.contains('\u{202e}'))
        );
        assert!(lines[0].starts_with(&format!("{} THE-1", IssueStatus::Todo.glyph())));
        assert!(
            lines
                .last()
                .is_some_and(|line| line.contains("comments incomplete"))
        );
        assert_eq!(
            render_text(&Outcome::Commented(issue_ref("home"))),
            vec!["Comment added to plugin:demo:1 (home)".to_owned()]
        );
    }
}
