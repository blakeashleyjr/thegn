//! Durable TUI handoff for one watched pull-request review task.
//!
//! The event loop only chooses an id and spawns this work. DB, git, forge and
//! agent operations all remain here on the blocking worker. Agent exit status
//! is advisory: resolution requires the exact local commit pushed from the
//! recorded baseline, a still-unresolved provider thread, and an expired
//! durable cooldown.

use std::path::Path;
use std::sync::{Mutex, OnceLock};

use thegn_core::agent_task::{TaskKind, TaskVars};
use thegn_core::config::{Config, PrQueueConfig};
use thegn_core::db::Db;
use thegn_core::forge::{Forge, ForgeError, PrRef, RepoRef};
use thegn_core::issue::{AgentDispatchStatus, ReviewTaskRecord};
use thegn_core::pr_review_tasks::ReviewTaskResolution;
use thegn_core::remote::GitLoc;
use thegn_core::store::NotificationStore;

#[derive(Debug, Clone)]
pub(crate) struct HandleContext {
    pub task_id: i64,
    pub pr_number: u64,
    pub repository: String,
    pub thread_id: String,
    pub path: String,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum HeadVerdict {
    Verified(String),
    NoMovement,
    Foreign(String),
}

/// Serialize explicit handle gestures. The roster API predates conditional
/// status updates, so this lock prevents two near-simultaneous TUI actions from
/// both observing `queued` and launching the same task.
fn handle_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub(crate) fn handle(cfg: &Config, context: &HandleContext) -> String {
    let Ok(_guard) = handle_lock().lock() else {
        return "review task handle is temporarily unavailable".into();
    };
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => return format!("review task DB unavailable: {error}"),
    };
    let task = match db.get_review_task(context.task_id) {
        Ok(Some(task)) => task,
        Ok(None) => return "review task no longer exists".into(),
        Err(error) => return format!("review task could not be loaded: {error}"),
    };
    let root = crate::integrate::main_checkout(Path::new(&task.worktree_path))
        .unwrap_or_else(|| Path::new(&task.worktree_path).to_path_buf());
    let queue = cfg.repo_pr_queue(&root);
    let loc = GitLoc::for_worktree(Path::new(&task.worktree_path));
    let forges = crate::forge_handle::get();
    let forge = forges.for_loc(&loc);
    handle_loaded(&db, cfg, &queue, forge, &loc, context, task)
}

fn handle_loaded(
    db: &Db,
    cfg: &Config,
    queue: &PrQueueConfig,
    forge: &dyn Forge,
    loc: &GitLoc,
    context: &HandleContext,
    task: ReviewTaskRecord,
) -> String {
    if task.status != AgentDispatchStatus::Queued {
        return format!("review task is {}, not queued", task.status.as_str());
    }
    if task.worktree_path.trim().is_empty() {
        return park(db, &task, "review task has no worktree to handle");
    }
    if cooldown_active(&task, thegn_core::util::now_ms()) {
        return park(db, &task, "review thread resolution is still cooling down");
    }
    if let Err(error) = db.update_review_task_status(task.id, AgentDispatchStatus::Running) {
        return format!("review task could not start durably: {error}");
    }

    // Exact configured role/command only. Unlike interactive review handoff,
    // durable tasks never fall back to the default agent role.
    let Some(command) = thegn_core::agent_task::resolve_agent(
        cfg,
        task.role.as_str(),
        queue.agent_command.as_str(),
    ) else {
        return park(
            db,
            &task,
            "configured review-task role/command could not be resolved",
        );
    };
    let before = match forge.pr_status(loc, PrRef::Number(context.pr_number)) {
        Ok(pr) => pr,
        Err(error) => return park_after_forge_error(db, &task, "pre-handoff PR refresh", &error),
    };
    if before.head_ref_oid != task.expected_head_oid {
        return park(
            db,
            &task,
            &format!(
                "PR head moved before handling (expected {}, found {})",
                task.expected_head_oid, before.head_ref_oid
            ),
        );
    }

    let sandbox = match crate::agent_run::agent_floor_gate(
        cfg,
        &task.worktree_path,
        queue.agent_sandbox,
        queue.agent_isolation_floor,
        queue.agent_on_floor_miss,
    ) {
        crate::agent_run::AgentDispatch::Run(sandbox) => sandbox,
        crate::agent_run::AgentDispatch::RunDegraded(sandbox, warning) => {
            thegn_core::msg::warn(&warning);
            sandbox
        }
        crate::agent_run::AgentDispatch::InfraHold(reason) => {
            return park(db, &task, &format!("review handoff blocked: {reason}"));
        }
    };
    let vars = TaskVars::new()
        .set("branch", &before.head_ref_name)
        .set("base", &before.base_ref_name)
        .set("worktree", &task.worktree_path)
        .set("pr_number", context.pr_number.to_string())
        .set("pr_url", &before.url)
        .set("pr_title", &before.title)
        .set("threads", "durable per-thread review task");
    let _agent_ok = crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind: TaskKind::PrReview,
        worktree: &task.worktree_path,
        prompt: &task.prompt,
        command_template: &command,
        vars: &vars,
        timeout_secs: queue.agent_timeout_secs,
        sandbox,
    });

    // A refresh may revise the same row while the agent is running. Never let
    // the old invocation resolve work it did not see; requeue the new revision.
    let current = match db.get_review_task(task.id) {
        Ok(Some(current)) => current,
        Ok(None) => return "review task disappeared while its agent was running".into(),
        Err(error) => return format!("review task could not be reloaded: {error}"),
    };
    if current.source_revision != task.source_revision {
        if let Err(error) = db.update_review_task_status(task.id, AgentDispatchStatus::Queued) {
            return format!("review task was revised but could not be requeued: {error}");
        }
        return "review task changed while running; latest revision requeued".into();
    }

    let local_head = loc.git_out(&["rev-parse", "HEAD"]);
    let after = match forge.pr_status(loc, PrRef::Number(context.pr_number)) {
        Ok(pr) => pr,
        Err(error) => return park_after_forge_error(db, &task, "post-agent PR refresh", &error),
    };
    let verified_head = match head_verdict(
        &task.expected_head_oid,
        &before.head_ref_oid,
        local_head.as_deref(),
        &after.head_ref_oid,
    ) {
        HeadVerdict::Verified(head) => head,
        HeadVerdict::NoMovement => {
            return park(db, &task, "agent exited without a verified PR head change");
        }
        HeadVerdict::Foreign(detail) => return park(db, &task, &detail),
    };

    let Some((owner, repo)) = context.repository.split_once('/') else {
        return park(db, &task, "review task repository identity is invalid");
    };
    let conversation = match forge.conversation(
        loc,
        &RepoRef {
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
        context.pr_number,
    ) {
        Ok(conversation) => conversation,
        Err(error) => {
            return park_after_forge_error(db, &task, "unresolved-thread recheck", &error);
        }
    };
    if context.thread_id == "review_decision" {
        return park(
            db,
            &task,
            "PR-level requested changes have no provider thread to resolve; re-review required",
        );
    }
    let Some(thread) = conversation
        .threads
        .iter()
        .find(|thread| thread.id == context.thread_id)
    else {
        return park(db, &task, "review thread was not returned by the provider");
    };
    if thread.resolved {
        return finish_resolved(db, &task, context, &verified_head);
    }
    let current = db
        .get_review_task(task.id)
        .ok()
        .flatten()
        .unwrap_or_else(|| task.clone());
    if cooldown_active(&current, thegn_core::util::now_ms()) {
        return park(db, &task, "review thread resolution is still cooling down");
    }

    let reply = format!(
        "thegn review task {} updated head {} for revision {}; please re-review.",
        task.id, verified_head, task.source_revision
    );
    match forge.resolve_review_thread(loc, &context.thread_id, &reply) {
        Ok(()) => finish_resolved(db, &task, context, &verified_head),
        Err(error) => park_after_forge_error(db, &task, "review thread resolve", &error),
    }
}

fn head_verdict(expected: &str, before: &str, local: Option<&str>, remote: &str) -> HeadVerdict {
    if before != expected {
        return HeadVerdict::Foreign(format!(
            "foreign PR head before task (expected {expected}, found {before})"
        ));
    }
    if remote == expected {
        return HeadVerdict::NoMovement;
    }
    match local {
        Some(local) if local == remote => HeadVerdict::Verified(remote.to_string()),
        Some(local) => HeadVerdict::Foreign(format!(
            "concurrent/foreign push detected (task worktree {local}, PR {remote})"
        )),
        None => HeadVerdict::Foreign(
            "could not verify the task worktree head against the moved PR".into(),
        ),
    }
}

fn cooldown_active(task: &ReviewTaskRecord, now_ms: i64) -> bool {
    task.next_forge_action_at_ms
        .is_some_and(|next| now_ms < next)
}

fn retry_at(error: &ForgeError, attempts: u32, now_ms: i64) -> Option<i64> {
    let seconds = match error {
        ForgeError::RateLimited => 15 * 60,
        ForgeError::Offline => 60u64.saturating_mul(1u64 << attempts.min(4)),
        ForgeError::Other(_) => 5 * 60,
        ForgeError::Unsupported(_)
        | ForgeError::NotAuthenticated
        | ForgeError::NotConfigured(_)
        | ForgeError::NotInstalled
        | ForgeError::NoPr => return None,
    };
    Some(now_ms.saturating_add((seconds as i64).saturating_mul(1_000)))
}

fn park_after_forge_error(
    db: &Db,
    task: &ReviewTaskRecord,
    operation: &str,
    error: &ForgeError,
) -> String {
    let reason = format!("{operation} failed: {}", error.describe());
    let next = retry_at(
        error,
        task.forge_action_attempts,
        thegn_core::util::now_ms(),
    );
    if let Err(persist_error) =
        db.record_review_forge_attempt(task.id, next, AgentDispatchStatus::WaitingHuman)
    {
        return format!("{reason}; durable backoff failed: {persist_error}");
    }
    notify_human(db, task, &reason);
    reason
}

fn park(db: &Db, task: &ReviewTaskRecord, reason: &str) -> String {
    if let Err(error) = db.update_review_task_status(task.id, AgentDispatchStatus::WaitingHuman) {
        return format!("{reason}; status persistence failed: {error}");
    }
    notify_human(db, task, reason);
    reason.to_string()
}

fn notify_human(db: &Db, task: &ReviewTaskRecord, reason: &str) {
    if let Err(error) = db.put_notification_once(
        thegn_core::notification::NotificationKind::PrQueueNeedsHuman.as_str(),
        &task.source_key,
        reason,
        &task.worktree_path,
    ) {
        tracing::warn!(target: "thegn::prq", %error, task = task.id, "review task human notification failed");
    }
}

fn finish_resolved(
    db: &Db,
    task: &ReviewTaskRecord,
    context: &HandleContext,
    head_oid: &str,
) -> String {
    let transition = ReviewTaskResolution {
        dispatch_id: task.id,
        source_key: task.source_key.clone(),
        source_revision: task.source_revision.clone(),
        forge: task
            .issue_id
            .strip_prefix("pr:")
            .and_then(|rest| rest.split_once(':'))
            .map(|(forge, _)| forge)
            .unwrap_or_default()
            .to_string(),
        repository: context.repository.clone(),
        pr_number: context.pr_number,
        thread_id: context.thread_id.clone(),
        path: context.path.clone(),
        line: context.line,
        head_oid: head_oid.to_string(),
        worktree_path: task.worktree_path.clone(),
    };
    match db.resolve_review_task(&transition) {
        Ok(true) | Ok(false) => {
            if let Err(error) = db.put_review_thread_resolved_notification(&transition) {
                tracing::warn!(target: "thegn::prq", %error, task = task.id, "review resolved notification failed");
            }
            format!("review thread {} resolved", context.thread_id)
        }
        Err(error) => format!("provider resolved the thread, but roster update failed: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeForge;
    impl thegn_core::seam::Probe for FakeForge {
        fn probe(&self) -> thegn_core::seam::ProbeReport {
            thegn_core::seam::ProbeReport::new(
                "forge",
                "fake",
                thegn_core::seam::Availability::Ready,
            )
        }
    }
    impl Forge for FakeForge {
        fn id(&self) -> &'static str {
            "fake"
        }
        fn caps(&self) -> thegn_core::forge::ForgeCaps {
            Default::default()
        }
        fn repo_ref(&self, _: &GitLoc) -> Option<thegn_core::forge::RepoRef> {
            None
        }
        fn pr_status(
            &self,
            _: &GitLoc,
            _: PrRef,
        ) -> Result<thegn_core::forge::model::PrStatus, ForgeError> {
            Err(ForgeError::Unsupported("pr_status"))
        }
        fn pr_list(
            &self,
            _: &GitLoc,
            _: usize,
        ) -> Result<Vec<thegn_core::forge::model::PrHeader>, ForgeError> {
            Ok(Vec::new())
        }
    }

    fn task(db: &Db) -> ReviewTaskRecord {
        use thegn_core::pr_review_tasks::{ReviewTaskEvent, ReviewTaskUpsert};
        let upsert = ReviewTaskUpsert {
            existing_id: None,
            issue_id: "pr:fake:acme/widget#22".into(),
            worktree_path: "/tmp/review-task".into(),
            role: "missing-role".into(),
            status: AgentDispatchStatus::Queued,
            source_key: "review_thread:sha256:test".into(),
            source_revision: "sha256:revision".into(),
            prompt: "fix the selected review thread".into(),
            expected_head_oid: "old-head".into(),
            event: ReviewTaskEvent {
                event: thegn_core::pr_review_tasks::REVIEW_THREAD_EVENT,
                source_key: "review_thread:sha256:test".into(),
                source_revision: "sha256:revision".into(),
                forge: "fake".into(),
                repository: "acme/widget".into(),
                pr_number: 22,
                pr_url: String::new(),
                pr_title: String::new(),
                branch: "feature".into(),
                base: "main".into(),
                head_oid: "old-head".into(),
                thread_id: "thread-22".into(),
                path: "src/lib.rs".into(),
                line: Some(9),
                role: "missing-role".into(),
                prompt: "fix the selected review thread".into(),
                worktree_path: "/tmp/review-task".into(),
            },
        };
        let id = db.upsert_review_task(&upsert).unwrap();
        db.get_review_task(id).unwrap().unwrap()
    }

    fn context(task: &ReviewTaskRecord) -> HandleContext {
        HandleContext {
            task_id: task.id,
            pr_number: 22,
            repository: "acme/widget".into(),
            thread_id: "thread-22".into(),
            path: "src/lib.rs".into(),
            line: Some(9),
        }
    }

    #[test]
    fn exact_expected_head_verification_rejects_foreign_pushes() {
        assert_eq!(
            head_verdict("a", "a", Some("b"), "b"),
            HeadVerdict::Verified("b".into())
        );
        assert_eq!(
            head_verdict("a", "a", Some("a"), "a"),
            HeadVerdict::NoMovement
        );
        assert!(matches!(
            head_verdict("a", "x", Some("b"), "b"),
            HeadVerdict::Foreign(_)
        ));
        assert!(matches!(
            head_verdict("a", "a", Some("b"), "c"),
            HeadVerdict::Foreign(_)
        ));
    }

    #[test]
    fn rate_limits_back_off_but_unsupported_never_auto_retries() {
        let now = 10_000;
        assert!(retry_at(&ForgeError::RateLimited, 0, now).unwrap() > now);
        assert!(retry_at(&ForgeError::Offline, 3, now).unwrap() > now);
        assert_eq!(
            retry_at(&ForgeError::Unsupported("resolve_review_thread"), 0, now),
            None
        );
        assert_eq!(retry_at(&ForgeError::NotAuthenticated, 0, now), None);
    }

    #[test]
    fn handle_without_configured_agent_transitions_to_waiting_human() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("handle.db")).unwrap();
        let task = task(&db);
        let message = handle_loaded(
            &db,
            &Config::default(),
            &PrQueueConfig::default(),
            &FakeForge,
            &GitLoc::Local(dir.path().to_path_buf()),
            &context(&task),
            task.clone(),
        );
        assert!(message.contains("could not be resolved"));
        assert_eq!(
            db.get_review_task(task.id).unwrap().unwrap().status,
            AgentDispatchStatus::WaitingHuman
        );
        assert!(!db.get_unread_notifications().unwrap().is_empty());
    }

    #[test]
    fn unsupported_and_rate_limited_resolve_attempts_remain_auditable() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join("resolve.db")).unwrap();
        let task = task(&db);
        let unsupported = park_after_forge_error(
            &db,
            &task,
            "review thread resolve",
            &ForgeError::Unsupported("resolve_review_thread"),
        );
        assert!(unsupported.contains("does not support"));
        let parked = db.get_review_task(task.id).unwrap().unwrap();
        assert_eq!(parked.status, AgentDispatchStatus::WaitingHuman);
        assert_eq!(parked.forge_action_attempts, 1);
        assert_eq!(parked.next_forge_action_at_ms, None);

        db.update_review_task_status(task.id, AgentDispatchStatus::Queued)
            .unwrap();
        let current = db.get_review_task(task.id).unwrap().unwrap();
        park_after_forge_error(
            &db,
            &current,
            "review thread resolve",
            &ForgeError::RateLimited,
        );
        assert!(
            db.get_review_task(task.id)
                .unwrap()
                .unwrap()
                .next_forge_action_at_ms
                .is_some()
        );
    }
}
