//! Pure derivation of durable, per-thread pull-request review tasks.
//!
//! Inputs are THE-27's cached review model plus the existing durable task
//! identities. Outputs are upserts/transitions and a bounded automation event;
//! this module performs no database, forge, process, terminal, or async work.

use crate::agent_task::{
    TaskKind, TaskVars, TemplateError, default_prompt, render_prompt, validate_template,
};
use crate::forge::model::{PrComment, PrReview, ReviewThread};
use crate::issue::{AgentDispatchStatus, ReviewTaskRecord};
use crate::review::{PrReviewSnapshot, format_review_feedback};
use sha2::{Digest, Sha256};

pub const REVIEW_TASK_KIND: &str = "pr_review";
pub const REVIEW_THREAD_EVENT: &str = "pr.thread_unresolved";
pub const MAX_REVIEW_TASK_PROMPT_CHARS: usize = 48 * 1024;
pub const MAX_REVIEW_EVENT_FIELD_CHARS: usize = 8 * 1024;
const MAX_IDENTITY_FIELD_CHARS: usize = 2 * 1024;
const MAX_REVISION_COMMENTS: usize = 64;

/// Stable, non-snapshot facts for one explicitly watched pull request.
#[derive(Debug, Clone, Copy)]
pub struct PrReviewTaskContext<'a> {
    pub forge: &'a str,
    pub repository: &'a str,
    pub pr_url: &'a str,
    pub pr_title: &'a str,
    pub base: &'a str,
    pub worktree_path: &'a str,
    pub role: &'a str,
    /// Empty selects the built-in [`TaskKind::PrReview`] prompt.
    pub prompt_template: &'a str,
}

/// The subset of a durable row needed by the pure reconciler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingReviewTask {
    pub id: i64,
    pub source_key: String,
    pub source_revision: String,
    pub content_revision: String,
    pub pending_source_revision: Option<String>,
    pub pending_content_revision: Option<String>,
    pub status: AgentDispatchStatus,
}

impl From<&ReviewTaskRecord> for ExistingReviewTask {
    fn from(row: &ReviewTaskRecord) -> Self {
        Self {
            id: row.id,
            source_key: row.source_key.clone(),
            source_revision: row.source_revision.clone(),
            content_revision: row.content_revision.clone(),
            pending_source_revision: row.pending_source_revision.clone(),
            pending_content_revision: row.pending_content_revision.clone(),
            status: row.status,
        }
    }
}

/// Bounded wire event emitted only after a create/revision is durably applied.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewTaskEvent {
    pub event: String,
    pub source_key: String,
    pub source_revision: String,
    pub forge: String,
    pub repository: String,
    pub pr_number: u64,
    pub pr_url: String,
    pub pr_title: String,
    pub branch: String,
    pub base: String,
    pub head_oid: String,
    pub thread_id: String,
    pub path: String,
    pub line: Option<u64>,
    pub role: String,
    pub prompt: String,
    pub worktree_path: String,
}

/// One insert/update of the review-task columns on `agent_dispatches`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTaskUpsert {
    pub existing_id: Option<i64>,
    pub issue_id: String,
    pub worktree_path: String,
    pub role: String,
    pub status: AgentDispatchStatus,
    pub source_key: String,
    pub source_revision: String,
    pub content_revision: String,
    pub prompt: String,
    pub expected_head_oid: String,
    pub event: ReviewTaskEvent,
}

/// A source observed resolved (or superseded by real threads).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTaskResolution {
    pub dispatch_id: i64,
    pub source_key: String,
    pub source_revision: String,
    pub forge: String,
    pub repository: String,
    pub pr_number: u64,
    pub thread_id: String,
    pub path: String,
    pub line: Option<u64>,
    pub head_oid: String,
    pub worktree_path: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewTaskPlan {
    pub upserts: Vec<ReviewTaskUpsert>,
    pub resolutions: Vec<ReviewTaskResolution>,
}

struct TaskDerivation<'a> {
    prior: Option<&'a ExistingReviewTask>,
    source_key: String,
    source_revision: String,
    content_revision: String,
    thread_id: &'a str,
    path: &'a str,
    line: Option<u64>,
    feedback: String,
}

/// Reconcile one successful snapshot. Callers must not invoke this for a
/// transient fetch failure: absence is not resolution, so the durable roster
/// remains untouched until a real snapshot arrives.
pub fn reconcile_review_tasks(
    snapshot: &PrReviewSnapshot,
    context: PrReviewTaskContext<'_>,
    existing: &[ExistingReviewTask],
) -> Result<ReviewTaskPlan, TemplateError> {
    let template = if context.prompt_template.trim().is_empty() {
        default_prompt(TaskKind::PrReview)
    } else {
        context.prompt_template
    };
    validate_template(template, TaskKind::PrReview.prompt_vars(), false)?;

    let mut plan = ReviewTaskPlan::default();
    let mut has_unresolved_thread = false;
    for thread in &snapshot.conversation.threads {
        if thread.id.trim().is_empty() {
            // An opaque provider id is mandatory. Never guess identity from an
            // anchor or comment body: both can legitimately be shared.
            continue;
        }
        let source_key = thread_source_key(&context, snapshot.pr_number, &thread.id);
        let prior = existing.iter().find(|task| task.source_key == source_key);
        if thread.resolved {
            if let Some(task) = prior.filter(|task| task.status != AgentDispatchStatus::Done) {
                plan.resolutions.push(resolution(
                    task,
                    snapshot,
                    &context,
                    &thread.id,
                    &thread.path,
                    thread.line,
                ));
            }
            continue;
        }
        has_unresolved_thread = true;
        let source_revision = thread_revision(snapshot, thread);
        let content_revision = thread_content_revision(thread);
        if prior.is_some_and(|task| {
            task.source_revision == source_revision
                || (matches!(
                    task.status,
                    AgentDispatchStatus::Spawning | AgentDispatchStatus::Running
                ) && task.pending_source_revision.as_deref() == Some(source_revision.as_str()))
        }) {
            continue;
        }
        let feedback = format_review_feedback(snapshot, Some(thread));
        plan.upserts.push(build_upsert(
            snapshot,
            &context,
            template,
            TaskDerivation {
                prior,
                source_key,
                source_revision,
                content_revision,
                thread_id: &thread.id,
                path: &thread.path,
                line: thread.line,
                feedback,
            },
        )?);
    }

    let decision_key = decision_source_key(&context, snapshot.pr_number);
    let prior_decision = existing.iter().find(|task| task.source_key == decision_key);
    // A PR-level requested-change body is a fallback only when the provider
    // supplied no thread objects at all. Real threads always own the work.
    let decision = (snapshot.conversation.threads.is_empty())
        .then(|| requested_change_review(snapshot))
        .flatten();
    if !has_unresolved_thread && let Some(review) = decision {
        let source_revision = decision_revision(snapshot, review);
        let content_revision = decision_content_revision(review);
        if prior_decision.is_none_or(|task| {
            task.source_revision != source_revision
                && !(matches!(
                    task.status,
                    AgentDispatchStatus::Spawning | AgentDispatchStatus::Running
                ) && task.pending_source_revision.as_deref() == Some(source_revision.as_str()))
        }) {
            let feedback = decision_feedback(snapshot, review);
            plan.upserts.push(build_upsert(
                snapshot,
                &context,
                template,
                TaskDerivation {
                    prior: prior_decision,
                    source_key: decision_key,
                    source_revision,
                    content_revision,
                    thread_id: "review_decision",
                    path: "PR-level",
                    line: None,
                    feedback,
                },
            )?);
        }
    } else if let Some(task) =
        prior_decision.filter(|task| task.status != AgentDispatchStatus::Done)
    {
        plan.resolutions.push(resolution(
            task,
            snapshot,
            &context,
            "review_decision",
            "PR-level",
            None,
        ));
    }
    Ok(plan)
}

fn build_upsert(
    snapshot: &PrReviewSnapshot,
    context: &PrReviewTaskContext<'_>,
    template: &str,
    derived: TaskDerivation<'_>,
) -> Result<ReviewTaskUpsert, TemplateError> {
    let clean = |value: &str| clean_bounded(value, MAX_REVIEW_EVENT_FIELD_CHARS);
    let vars = TaskVars::new()
        .set("branch", clean(&snapshot.branch))
        .set("base", clean(context.base))
        .set("worktree", clean(context.worktree_path))
        .set("pr_number", snapshot.pr_number.to_string())
        .set("pr_url", clean(context.pr_url))
        .set("pr_title", clean(context.pr_title))
        .set("threads", derived.feedback);
    let prompt = clean_bounded(
        &render_prompt(template, &vars)?,
        MAX_REVIEW_TASK_PROMPT_CHARS,
    );
    let status = derived
        .prior
        .map_or(AgentDispatchStatus::Queued, |task| match task.status {
            AgentDispatchStatus::Spawning | AgentDispatchStatus::Running => task.status,
            // A new revision is new actionable work. Human-parked and terminal
            // rows return to the queue; an unchanged revision was filtered above.
            _ => AgentDispatchStatus::Queued,
        });
    let event = ReviewTaskEvent {
        event: REVIEW_THREAD_EVENT.to_string(),
        source_key: derived.source_key.clone(),
        source_revision: derived.source_revision.clone(),
        forge: clean(context.forge),
        repository: clean(context.repository),
        pr_number: snapshot.pr_number,
        pr_url: clean(context.pr_url),
        pr_title: clean(context.pr_title),
        branch: clean(&snapshot.branch),
        base: clean(context.base),
        head_oid: clean(&snapshot.head_oid),
        thread_id: clean(derived.thread_id),
        path: clean(derived.path),
        line: derived.line,
        role: clean(context.role),
        prompt: prompt.clone(),
        worktree_path: clean(context.worktree_path),
    };
    Ok(ReviewTaskUpsert {
        existing_id: derived.prior.map(|task| task.id),
        issue_id: format!(
            "pr:{}:{}#{}",
            clean_bounded(context.forge, 128),
            clean_bounded(context.repository, 512),
            snapshot.pr_number
        ),
        worktree_path: clean(context.worktree_path),
        role: clean(context.role),
        status,
        source_key: derived.source_key,
        source_revision: derived.source_revision,
        content_revision: derived.content_revision,
        prompt,
        expected_head_oid: clean(&snapshot.head_oid),
        event,
    })
}

fn resolution(
    task: &ExistingReviewTask,
    snapshot: &PrReviewSnapshot,
    context: &PrReviewTaskContext<'_>,
    thread_id: &str,
    path: &str,
    line: Option<u64>,
) -> ReviewTaskResolution {
    ReviewTaskResolution {
        dispatch_id: task.id,
        source_key: task.source_key.clone(),
        source_revision: task.source_revision.clone(),
        forge: clean_bounded(context.forge, MAX_REVIEW_EVENT_FIELD_CHARS),
        repository: clean_bounded(context.repository, MAX_REVIEW_EVENT_FIELD_CHARS),
        pr_number: snapshot.pr_number,
        thread_id: clean_bounded(thread_id, MAX_REVIEW_EVENT_FIELD_CHARS),
        path: clean_bounded(path, MAX_REVIEW_EVENT_FIELD_CHARS),
        line,
        head_oid: clean_bounded(&snapshot.head_oid, MAX_REVIEW_EVENT_FIELD_CHARS),
        worktree_path: clean_bounded(context.worktree_path, MAX_REVIEW_EVENT_FIELD_CHARS),
    }
}

pub fn thread_source_key(
    context: &PrReviewTaskContext<'_>,
    pr_number: u64,
    thread_id: &str,
) -> String {
    source_key("review_thread", context, pr_number, thread_id)
}

pub fn decision_source_key(context: &PrReviewTaskContext<'_>, pr_number: u64) -> String {
    source_key("review_decision", context, pr_number, "")
}

fn source_key(
    kind: &str,
    context: &PrReviewTaskContext<'_>,
    pr_number: u64,
    source_id: &str,
) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, kind.as_bytes());
    hash_field(&mut hasher, context.forge.as_bytes());
    hash_field(&mut hasher, context.repository.as_bytes());
    hash_field(&mut hasher, pr_number.to_string().as_bytes());
    hash_field(&mut hasher, source_id.as_bytes());
    format!("{kind}:sha256:{:x}", hasher.finalize())
}

fn thread_revision(snapshot: &PrReviewSnapshot, thread: &ReviewThread) -> String {
    let mut canonical = thread_revision_prefix(thread);
    push_canonical(
        &mut canonical,
        "head",
        &snapshot.head_oid,
        MAX_IDENTITY_FIELD_CHARS,
    );
    digest(&canonical)
}

/// Revision of review feedback independent of the PR head. An active handoff
/// can therefore accept a refresh that observed its own verified push while
/// still rejecting a genuinely new comment or anchor change.
pub fn thread_content_revision(thread: &ReviewThread) -> String {
    digest(&thread_revision_prefix(thread))
}

fn thread_revision_prefix(thread: &ReviewThread) -> String {
    let mut canonical = String::new();
    push_canonical(
        &mut canonical,
        "thread",
        &thread.id,
        MAX_IDENTITY_FIELD_CHARS,
    );
    push_canonical(
        &mut canonical,
        "resolved",
        if thread.resolved { "1" } else { "0" },
        1,
    );
    push_canonical(
        &mut canonical,
        "path",
        &thread.path,
        MAX_REVIEW_EVENT_FIELD_CHARS,
    );
    push_canonical(
        &mut canonical,
        "line",
        &thread
            .line
            .map_or_else(String::new, |line| line.to_string()),
        32,
    );
    push_canonical(
        &mut canonical,
        "hunk",
        &thread.diff_hunk,
        MAX_REVIEW_EVENT_FIELD_CHARS,
    );
    push_canonical(
        &mut canonical,
        "comment_count",
        &thread.comments.len().to_string(),
        32,
    );
    for comment in thread.comments.iter().take(MAX_REVISION_COMMENTS) {
        push_comment(&mut canonical, comment);
    }
    digest(&canonical)
}

fn decision_revision(snapshot: &PrReviewSnapshot, review: &PrReview) -> String {
    let mut canonical = String::new();
    push_canonical(&mut canonical, "kind", "review_decision", 32);
    push_canonical(
        &mut canonical,
        "head",
        &snapshot.head_oid,
        MAX_IDENTITY_FIELD_CHARS,
    );
    push_canonical(&mut canonical, "author", &review.author, 256);
    push_canonical(&mut canonical, "state", &review.state, 64);
    push_canonical(
        &mut canonical,
        "submitted",
        &review.submitted_at,
        MAX_IDENTITY_FIELD_CHARS,
    );
    push_canonical(
        &mut canonical,
        "body",
        &review.body,
        MAX_REVIEW_EVENT_FIELD_CHARS,
    );
    digest(&canonical)
}

fn decision_content_revision(review: &PrReview) -> String {
    let mut canonical = String::new();
    push_canonical(&mut canonical, "kind", "review_decision", 32);
    push_canonical(&mut canonical, "author", &review.author, 256);
    push_canonical(&mut canonical, "state", &review.state, 64);
    push_canonical(
        &mut canonical,
        "submitted",
        &review.submitted_at,
        MAX_IDENTITY_FIELD_CHARS,
    );
    push_canonical(
        &mut canonical,
        "body",
        &review.body,
        MAX_REVIEW_EVENT_FIELD_CHARS,
    );
    digest(&canonical)
}

fn push_comment(canonical: &mut String, comment: &PrComment) {
    push_canonical(
        canonical,
        "comment_id",
        &comment.id,
        MAX_IDENTITY_FIELD_CHARS,
    );
    push_canonical(canonical, "author", &comment.author, 256);
    push_canonical(
        canonical,
        "created",
        &comment.created_at,
        MAX_IDENTITY_FIELD_CHARS,
    );
    push_canonical(
        canonical,
        "body",
        &comment.body,
        MAX_REVIEW_EVENT_FIELD_CHARS,
    );
}

fn push_canonical(out: &mut String, label: &str, value: &str, max_chars: usize) {
    let value = clean_bounded(value, max_chars);
    out.push_str(label);
    out.push(':');
    out.push_str(&value.len().to_string());
    out.push(':');
    out.push_str(&value);
    out.push('\n');
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update(value.len().to_be_bytes());
    hasher.update(value);
}

fn digest(canonical: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

fn requested_change_review(snapshot: &PrReviewSnapshot) -> Option<&PrReview> {
    snapshot.conversation.reviews.iter().rev().find(|review| {
        review.state.eq_ignore_ascii_case("CHANGES_REQUESTED") && !review.body.trim().is_empty()
    })
}

fn decision_feedback(snapshot: &PrReviewSnapshot, review: &PrReview) -> String {
    let mut decision_snapshot = snapshot.clone();
    decision_snapshot.conversation.comments.clear();
    decision_snapshot.conversation.threads.clear();
    decision_snapshot.conversation.reviews = vec![review.clone()];
    format!(
        "Location: PR-level\n{}",
        format_review_feedback(&decision_snapshot, None)
    )
}

fn clean_bounded(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|ch| !matches!(*ch as u32, 0x00..=0x08 | 0x0b..=0x1f | 0x7f..=0x9f))
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::model::{PrConversation, PrReview};

    fn context<'a>() -> PrReviewTaskContext<'a> {
        PrReviewTaskContext {
            forge: "github",
            repository: "acme/widget",
            pr_url: "https://github.com/acme/widget/pull/22",
            pr_title: "Handle feedback",
            base: "main",
            worktree_path: "/wt/feature",
            role: "coder",
            prompt_template: default_prompt(TaskKind::PrReview),
        }
    }

    fn thread(resolved: bool, body: &str) -> ReviewThread {
        ReviewThread {
            id: "PRRT_kwDOabc".into(),
            path: "src/lib.rs".into(),
            line: Some(42),
            resolved,
            comments: vec![PrComment {
                id: "C1".into(),
                author: "reviewer".into(),
                body: body.into(),
                created_at: "2026-08-30T12:00:00Z".into(),
            }],
            diff_hunk: "@@ -40,1 +40,2 @@".into(),
        }
    }

    fn snapshot(thread: ReviewThread) -> PrReviewSnapshot {
        PrReviewSnapshot {
            worktree_key: "/wt/feature".into(),
            branch: "feature".into(),
            pr_number: 22,
            head_oid: "abc123".into(),
            conversation: PrConversation {
                threads: vec![thread],
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn existing(upsert: &ReviewTaskUpsert, status: AgentDispatchStatus) -> ExistingReviewTask {
        ExistingReviewTask {
            id: 7,
            source_key: upsert.source_key.clone(),
            source_revision: upsert.source_revision.clone(),
            content_revision: upsert.content_revision.clone(),
            pending_source_revision: None,
            pending_content_revision: None,
            status,
        }
    }

    #[test]
    fn identical_thread_is_a_no_op() {
        let snapshot = snapshot(thread(false, "please change this"));
        let first = reconcile_review_tasks(&snapshot, context(), &[]).unwrap();
        let prior = existing(&first.upserts[0], AgentDispatchStatus::Queued);
        let second = reconcile_review_tasks(&snapshot, context(), &[prior]).unwrap();
        assert!(second.upserts.is_empty());
        assert!(second.resolutions.is_empty());
    }

    #[test]
    fn new_comment_revises_the_same_row_without_a_concurrent_run() {
        let before = snapshot(thread(false, "first"));
        let first = reconcile_review_tasks(&before, context(), &[]).unwrap();
        let prior = existing(&first.upserts[0], AgentDispatchStatus::Running);
        let mut after = before.clone();
        after.conversation.threads[0].comments.push(PrComment {
            id: "C2".into(),
            body: "follow-up".into(),
            ..Default::default()
        });
        let revised = reconcile_review_tasks(&after, context(), &[prior]).unwrap();
        assert_eq!(revised.upserts.len(), 1);
        assert_eq!(revised.upserts[0].existing_id, Some(7));
        assert_eq!(revised.upserts[0].status, AgentDispatchStatus::Running);
        assert_ne!(
            revised.upserts[0].source_revision,
            first.upserts[0].source_revision
        );
    }

    #[test]
    fn active_refresh_emits_each_pending_revision_only_once() {
        let before = snapshot(thread(false, "first"));
        let first = reconcile_review_tasks(&before, context(), &[]).unwrap();
        let mut after = before.clone();
        after.head_oid = "new-head".into();
        let pending = reconcile_review_tasks(
            &after,
            context(),
            &[ExistingReviewTask {
                id: 7,
                source_key: first.upserts[0].source_key.clone(),
                source_revision: first.upserts[0].source_revision.clone(),
                content_revision: first.upserts[0].content_revision.clone(),
                pending_source_revision: None,
                pending_content_revision: None,
                status: AgentDispatchStatus::Running,
            }],
        )
        .unwrap();
        assert_eq!(pending.upserts.len(), 1);
        let unchanged = reconcile_review_tasks(
            &after,
            context(),
            &[ExistingReviewTask {
                id: 7,
                source_key: pending.upserts[0].source_key.clone(),
                source_revision: first.upserts[0].source_revision.clone(),
                content_revision: first.upserts[0].content_revision.clone(),
                pending_source_revision: Some(pending.upserts[0].source_revision.clone()),
                pending_content_revision: Some(pending.upserts[0].content_revision.clone()),
                status: AgentDispatchStatus::Running,
            }],
        )
        .unwrap();
        assert!(unchanged.upserts.is_empty());
    }

    #[test]
    fn content_revision_ignores_head_but_changes_for_feedback() {
        let first = snapshot(thread(false, "first"));
        let mut moved = first.clone();
        moved.head_oid = "other-head".into();
        assert_eq!(
            thread_content_revision(&first.conversation.threads[0]),
            thread_content_revision(&moved.conversation.threads[0])
        );
        moved.conversation.threads[0].comments[0].body = "new comment".into();
        assert_ne!(
            thread_content_revision(&first.conversation.threads[0]),
            thread_content_revision(&moved.conversation.threads[0])
        );
    }

    #[test]
    fn resolved_thread_transitions_the_existing_task() {
        let before = snapshot(thread(false, "fix"));
        let first = reconcile_review_tasks(&before, context(), &[]).unwrap();
        let prior = existing(&first.upserts[0], AgentDispatchStatus::Running);
        let resolved = snapshot(thread(true, "fix"));
        let plan = reconcile_review_tasks(&resolved, context(), &[prior]).unwrap();
        assert!(plan.upserts.is_empty());
        assert_eq!(plan.resolutions.len(), 1);
        assert_eq!(plan.resolutions[0].dispatch_id, 7);
    }

    #[test]
    fn thread_and_review_decision_have_distinct_source_identity() {
        let context = context();
        assert_ne!(
            thread_source_key(&context, 22, "review_decision"),
            decision_source_key(&context, 22)
        );
        let mut decision = snapshot(thread(false, "unused"));
        decision.conversation.threads.clear();
        decision.conversation.reviews = vec![PrReview {
            state: "CHANGES_REQUESTED".into(),
            body: "Please add a test".into(),
            ..Default::default()
        }];
        let plan = reconcile_review_tasks(&decision, context, &[]).unwrap();
        assert_eq!(plan.upserts.len(), 1);
        assert_eq!(plan.upserts[0].event.thread_id, "review_decision");
        assert!(plan.upserts[0].prompt.contains("Location: PR-level"));
    }

    #[test]
    fn prompt_and_event_remote_text_are_bounded_and_sanitized() {
        let hostile = format!("{}\u{1b}[2J", "x".repeat(MAX_REVIEW_TASK_PROMPT_CHARS * 2));
        let snapshot = snapshot(thread(false, &hostile));
        let plan = reconcile_review_tasks(&snapshot, context(), &[]).unwrap();
        let task = &plan.upserts[0];
        assert!(task.prompt.chars().count() <= MAX_REVIEW_TASK_PROMPT_CHARS);
        assert!(!task.prompt.contains('\u{1b}'));
        for field in [
            &task.event.pr_title,
            &task.event.thread_id,
            &task.event.path,
            &task.event.prompt,
        ] {
            assert!(!field.chars().any(|ch| ch == '\u{1b}'));
        }
    }

    #[test]
    fn revision_digest_is_deterministic_and_head_sensitive() {
        let snapshot = snapshot(thread(false, "body"));
        let one = reconcile_review_tasks(&snapshot, context(), &[]).unwrap();
        let two = reconcile_review_tasks(&snapshot, context(), &[]).unwrap();
        assert_eq!(
            one.upserts[0].source_revision,
            two.upserts[0].source_revision
        );
        assert!(one.upserts[0].source_revision.starts_with("sha256:"));
        let mut moved = snapshot.clone();
        moved.head_oid = "def456".into();
        let three = reconcile_review_tasks(&moved, context(), &[]).unwrap();
        assert_ne!(
            one.upserts[0].source_revision,
            three.upserts[0].source_revision
        );
    }
}
