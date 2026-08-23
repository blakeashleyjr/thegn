//! The PR-queue driver: refresh each queued pull request, work out what is
//! blocking it, and act — or explain why it didn't.
//!
//! The team-mode counterpart to [`crate::merge_driver`], and deliberately the
//! same shape: a synchronous loop that runs **off** the event loop (the CLI
//! calls it directly; the host runs it from `spawn_blocking`), writing status
//! transitions as it goes and reporting each through a `progress` callback the
//! caller uses to print (CLI) or repaint (host).
//!
//! What it does NOT contain is any policy. Every "may we act" question —
//! don't stomp a teammate's push, don't merge unreviewed work, don't touch
//! someone else's PR — is decided by the pure [`thegn_core::pr_queue`] layer and
//! merely executed here. That split is what makes the safety rules testable
//! without a forge.

use std::path::Path;

use thegn_core::agent_task::{TaskKind, TaskVars};
use thegn_core::config::{Config, PrMergeMethod, PrQueueConfig};
use thegn_core::db::{Db, PrQueueRow};
use thegn_core::forge::model::{MergeMethod, ReviewThreadRow};
use thegn_core::forge::{FetchedPr, Forge, ForgeError, PrRef};
use thegn_core::pr_queue::{self, Blocker, PrQueueFacts, PrqStatus, QueueAction};
use thegn_core::remote::GitLoc;
use thegn_core::store::WorktreeAuxStore;

/// One queued pull request to process.
#[derive(Debug, Clone)]
pub(crate) struct PrItem {
    pub key: String,
    pub number: u64,
    pub branch: String,
    pub worktree: Option<String>,
    pub agent_attempts: u32,
    pub last_head_oid: Option<String>,
    pub forge: String,
}

impl From<&PrQueueRow> for PrItem {
    fn from(r: &PrQueueRow) -> Self {
        PrItem {
            key: r.key.clone(),
            number: r.number,
            branch: r.branch.clone(),
            worktree: r.worktree.clone(),
            agent_attempts: r.agent_attempts,
            last_head_oid: r.last_head_oid.clone(),
            forge: r.forge.clone(),
        }
    }
}

/// One status transition, handed to the caller's `progress` callback (the DB row
/// is already written when this fires).
pub(crate) struct PrStep<'a> {
    pub key: &'a str,
    pub number: u64,
    pub branch: &'a str,
    pub status: &'a str,
    pub detail: &'a str,
}

/// Summary of one pass over the queue.
#[derive(Debug, Default, Clone)]
pub(crate) struct PrOutcome {
    /// Merged, or handed to the forge's auto-merge.
    pub merged: Vec<u64>,
    /// Green and mergeable, held for a human (`merge_mode = "ready"`).
    pub ready: Vec<u64>,
    /// Still blocked; will be re-examined next pass.
    pub blocked: Vec<u64>,
    /// Needs a person.
    pub needs_human: Vec<u64>,
    /// Left the queue (merged or closed on the forge).
    pub dropped: Vec<u64>,
    /// Non-fatal problems worth surfacing — an unreachable forge, an agent that
    /// was configured but could not be resolved. Reported rather than swallowed:
    /// "couldn't look" must never read as "nothing to do".
    pub warnings: Vec<String>,
}

/// Queue rows belonging to one repo, oldest first.
pub(crate) fn rows_for_repo(db: &Db, root: &Path) -> Vec<PrQueueRow> {
    let root_s = root.to_string_lossy();
    db.list_pr_queue()
        .unwrap_or_default()
        .into_iter()
        .filter(|r| r.repo_root == root_s)
        .collect()
}

// --- fetch health: per-row failure backoff ---------------------------------
//
// A forge that is down (or rate-limiting) must not be re-hammered every tick.
// Same shape as `ci_refresh`'s: consecutive failures double the wait, capped,
// and a success clears it. Keyed by row so one unreachable PR never stalls the
// rest of the queue.

/// `key → (consecutive failures, epoch seconds until which to skip)`.
fn health() -> &'static std::sync::Mutex<std::collections::HashMap<String, (u32, i64)>> {
    static H: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, (u32, i64)>>> =
        std::sync::OnceLock::new();
    H.get_or_init(Default::default)
}

/// Whether this row is inside its backoff window.
fn backoff_active(key: &str, now: i64) -> bool {
    health()
        .lock()
        .ok()
        .and_then(|m| m.get(key).map(|(_, until)| now < *until))
        .unwrap_or(false)
}

/// Record a failed fetch and arm the next window. Returns the wait in seconds.
fn record_failure(key: &str, now: i64, poll_secs: u64) -> u64 {
    let Ok(mut m) = health().lock() else { return 0 };
    let e = m.entry(key.to_string()).or_insert((0, 0));
    e.0 = e.0.saturating_add(1);
    let wait = crate::ci_refresh::backoff_secs(e.0, poll_secs);
    e.1 = now + wait as i64;
    wait
}

/// Clear a row's failure state after a good fetch.
fn record_success(key: &str) {
    if let Ok(mut m) = health().lock() {
        m.remove(key);
    }
}

/// Process every queued pull request once.
///
/// Best-effort DB writes throughout (the DB is a cache; the forge is the source
/// of truth), which is why the status writes use `let _ =`.
pub(crate) fn drive_queue(
    cfg: &PrQueueConfig,
    full: &Config,
    forge: &dyn Forge,
    repo_root: &Path,
    db: &Db,
    items: Vec<PrItem>,
    mut progress: impl FnMut(&PrStep),
) -> PrOutcome {
    let mut out = PrOutcome::default();

    // Resolved once per pass, like the merge queue's.
    let agent_cmd = thegn_core::agent_task::resolve_agent(full, &cfg.agent, &cfg.agent_command);
    if agent_cmd.is_none() && !cfg.agent.trim().is_empty() {
        let msg = format!(
            "pr_queue.agent = {:?} matches no [[agents]]/[[tools]] entry; \
             blockers will be reported but not fixed",
            cfg.agent
        );
        tracing::warn!(target: "thegn::prq", "{msg}");
        out.warnings.push(msg);
    }
    let me = viewer_login(forge, repo_root);

    let now = thegn_core::util::now();
    for item in items {
        // Still inside a failure window — skip without touching the row, so a
        // forge outage degrades to "stale" rather than to a queue full of
        // "unreachable" notes.
        if backoff_active(&item.key, now) {
            continue;
        }
        let step = |status: &str, detail: &str, progress: &mut dyn FnMut(&PrStep)| {
            progress(&PrStep {
                key: &item.key,
                number: item.number,
                branch: &item.branch,
                status,
                detail,
            });
        };

        // Where to run forge commands: the PR's own worktree when it has one,
        // else the repo root — `gh` only needs *a* checkout of the repo, so a PR
        // with no worktree is still fetchable. `None` location = local.
        let loc = GitLoc::from_db(
            &item
                .worktree
                .clone()
                .unwrap_or_else(|| repo_root.to_string_lossy().into_owned()),
            None,
        );

        if item.forge != forge.id() {
            // The row was queued under another forge id (a repo whose origin
            // moved, or a hand-edited row). The resolved forge still serves
            // it — `fetch` will say `NoPr` if it really isn't there.
            tracing::warn!(
                target: "thegn::prq",
                row_forge = %item.forge,
                resolved = forge.id(),
                number = item.number,
                "queue row forge id differs from the resolved forge"
            );
        }
        let fetched = match forge.fetch_pr(&loc, item.number) {
            Ok(f) => f,
            Err(e) => {
                // A fetch failure is a fact about the network, never a verdict
                // about the PR — leave the row's status and last known head
                // alone so `foreign_push` still has something to compare, and
                // let the poller's backoff handle the retry.
                let wait = record_failure(&item.key, now, cfg.poll_secs());
                let msg = format!(
                    "PR #{}: {} — retrying in {wait}s",
                    item.number,
                    describe(&e)
                );
                tracing::warn!(target: "thegn::prq", "{msg}");
                let _ = db.update_pr_status(
                    &item.key,
                    &current_status(db, &item.key),
                    None,
                    Some(&msg),
                    None,
                );
                out.warnings.push(msg);
                continue;
            }
        };

        record_success(&item.key);
        let head = fetched.pr.head_ref_oid.clone();
        let blocker = pr_queue::classify(&fetched.pr, cfg);

        // A head thegn didn't produce means a teammate pushed. Two consequences,
        // both deliberate: the attempt budget refills (a long-lived PR must not
        // stay stuck), and — under `pause_on_foreign_push` — we stop rather than
        // race whoever is working in there.
        let moved = pr_queue::foreign_push(item.last_head_oid.as_deref(), &head);
        let mut attempts = item.agent_attempts;
        if moved && pr_queue::attempts_reset(item.last_head_oid.as_deref(), &head, cfg) {
            attempts = 0;
            let _ = db.set_pr_agent_attempts(&item.key, 0);
        }

        let facts = PrQueueFacts {
            worktree: item.worktree.clone(),
            agent_attempts: attempts,
            last_head_oid: item.last_head_oid.clone(),
            is_own: is_own_pr(&fetched, me.as_deref()),
            agent_available: agent_cmd.is_some(),
        };

        let action = pr_queue::decide(&blocker, &facts, cfg);

        // The foreign-push guard applies only where it means something: we are
        // about to *write*. Merging a PR someone else advanced is fine (it is
        // still green); pushing over them is not.
        let action = if moved
            && cfg.pause_on_foreign_push
            && matches!(action, QueueAction::DispatchAgent(_))
        {
            QueueAction::NeedsHuman(
                "someone else pushed to this branch — not running an agent over their work".into(),
            )
        } else {
            action
        };

        match action {
            QueueAction::Drop => {
                let status = if fetched.pr.state.eq_ignore_ascii_case("MERGED") {
                    PrqStatus::Merged
                } else {
                    PrqStatus::Closed
                };
                let _ = db.update_pr_status(
                    &item.key,
                    status.as_str(),
                    Some(blocker.as_str()),
                    Some(&format!("{} on the forge", status.as_str())),
                    Some(&head),
                );
                step(status.as_str(), "", &mut progress);
                out.dropped.push(item.number);
            }

            QueueAction::Wait => {
                let status = PrqStatus::for_blocker(&blocker);
                let detail = blocker_detail(&blocker, &fetched);
                let _ = db.update_pr_status(
                    &item.key,
                    status.as_str(),
                    Some(blocker.as_str()),
                    Some(&detail),
                    Some(&head),
                );
                step(status.as_str(), &detail, &mut progress);
                if !matches!(blocker, Blocker::None) {
                    out.blocked.push(item.number);
                }
            }

            QueueAction::MarkReady => {
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::Ready.as_str(),
                    Some(blocker.as_str()),
                    Some("green and mergeable"),
                    Some(&head),
                );
                step(
                    PrqStatus::Ready.as_str(),
                    "green and mergeable",
                    &mut progress,
                );
                out.ready.push(item.number);
            }

            QueueAction::EnableAutoMerge | QueueAction::Merge => {
                let direct = matches!(action, QueueAction::Merge);
                let method = merge_method(cfg.merge_method);
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::Merging.as_str(),
                    None,
                    None,
                    Some(&head),
                );
                step(PrqStatus::Merging.as_str(), "", &mut progress);

                // Direct merge now, or ask the forge to merge itself once its
                // own rules allow (branch protection + required reviews stay
                // in charge) — the same op with `auto` flipped.
                let res = forge.merge_pr(
                    &loc,
                    PrRef::Number(item.number),
                    method,
                    cfg.delete_branch_on_merge,
                    !direct,
                );
                match res {
                    Ok(()) => {
                        // Direct merge is done; auto-merge is *armed* — the forge
                        // merges it when its own rules allow, and the next poll
                        // observes that. Saying "merged" now would be a lie.
                        let (status, detail) = if direct {
                            (PrqStatus::Merged, "merged".to_string())
                        } else {
                            (
                                PrqStatus::Ready,
                                "auto-merge enabled — the forge will merge it".to_string(),
                            )
                        };
                        let _ = db.update_pr_status(
                            &item.key,
                            status.as_str(),
                            Some(blocker.as_str()),
                            Some(&detail),
                            Some(&head),
                        );
                        step(status.as_str(), &detail, &mut progress);
                        out.merged.push(item.number);
                    }
                    Err(e) => {
                        // A refused merge is usually the forge enforcing a rule
                        // thegn cannot see (a required check, a CODEOWNERS
                        // review). That is a human's problem, not a retry.
                        let detail = format!("merge refused: {}", describe(&e));
                        let _ = db.update_pr_status(
                            &item.key,
                            PrqStatus::NeedsHuman.as_str(),
                            Some(blocker.as_str()),
                            Some(&detail),
                            Some(&head),
                        );
                        step(PrqStatus::NeedsHuman.as_str(), &detail, &mut progress);
                        out.needs_human.push(item.number);
                    }
                }
            }

            QueueAction::NeedsHuman(reason) => {
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::NeedsHuman.as_str(),
                    Some(blocker.as_str()),
                    Some(&reason),
                    Some(&head),
                );
                step(PrqStatus::NeedsHuman.as_str(), &reason, &mut progress);
                out.needs_human.push(item.number);
            }

            QueueAction::DispatchAgent(kind) => {
                // Before waking an agent on a red build, try the cheap thing: a
                // lot of red CI is a flake, and a re-run costs nothing but a
                // little wall-clock.
                if kind == TaskKind::PrCiFailure
                    && attempts == 0
                    && let Ok(n) = forge.rerun_failed(&loc, PrRef::Number(item.number))
                    && n > 0
                {
                    {
                        let detail = format!("re-ran {n} failed check(s) before dispatching");
                        let _ = db.update_pr_status(
                            &item.key,
                            PrqStatus::BlockedCi.as_str(),
                            Some(blocker.as_str()),
                            Some(&detail),
                            Some(&head),
                        );
                        step(PrqStatus::BlockedCi.as_str(), &detail, &mut progress);
                        out.blocked.push(item.number);
                        continue;
                    }
                }

                let next = attempts + 1;
                let _ = db.set_pr_agent_attempts(&item.key, next);
                let note = format!("agent fixing ({next}/{})", cfg.agent_max_attempts);
                let _ = db.update_pr_status(
                    &item.key,
                    PrqStatus::AgentRunning.as_str(),
                    Some(blocker.as_str()),
                    Some(&note),
                    Some(&head),
                );
                step(PrqStatus::AgentRunning.as_str(), &note, &mut progress);

                // Safe: `decide` returns DispatchAgent only with a worktree and a
                // resolved command.
                let (Some(wt), Some(template)) = (item.worktree.as_deref(), agent_cmd.as_deref())
                else {
                    continue;
                };
                run_agent(cfg, kind, template, wt, &item, &fetched, &blocker);

                // The exit code decides nothing — the next refresh does, exactly
                // as in the merge queue. An agent can exit non-zero having pushed
                // a good fix, or exit zero having done nothing.
            }
        }
    }
    out
}

/// The row's current status word, for a refresh that failed and must not change it.
fn current_status(db: &Db, key: &str) -> String {
    db.list_pr_queue()
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.key == key)
        .map(|r| r.status)
        .unwrap_or_else(|| PrqStatus::Watching.as_str().to_string())
}

/// A one-line human detail for a blocker.
fn blocker_detail(b: &Blocker, f: &FetchedPr) -> String {
    match b {
        Blocker::None => "green and mergeable".into(),
        Blocker::Draft => "draft — mark it ready for review".into(),
        Blocker::Ci(names) => format!("failing: {}", names.join(", ")),
        Blocker::ChecksPending => "checks still running".into(),
        Blocker::Conflict => format!("conflicts with / behind {}", f.pr.base_ref_name),
        Blocker::ChangesRequested => {
            let n = f.threads.iter().filter(|t| !t.resolved).count();
            format!("changes requested ({n} unresolved thread(s))")
        }
        Blocker::AwaitingReview => "awaiting review".into(),
        Blocker::Closed => "closed on the forge".into(),
    }
}

fn merge_method(m: PrMergeMethod) -> MergeMethod {
    match m {
        PrMergeMethod::Squash => MergeMethod::Squash,
        PrMergeMethod::Merge => MergeMethod::Merge,
        PrMergeMethod::Rebase => MergeMethod::Rebase,
    }
}

/// Whether the session's user authored this PR.
///
/// Unknown viewer ⇒ **not** ours. That is the safe direction: `own_prs_only`
/// then blocks the agent rather than letting it write to a colleague's PR
/// because `gh` happened to be unreadable.
fn is_own_pr(f: &FetchedPr, me: Option<&str>) -> bool {
    let Some(me) = me.filter(|m| !m.is_empty()) else {
        return false;
    };
    // `PrStatus` carries no author field, so fall back to the URL's owner only
    // when the PR lives in the viewer's own namespace. Threads authored by the
    // viewer are not evidence of authorship.
    f.pr.url
        .split('/')
        .nth(3)
        .is_some_and(|owner| owner.eq_ignore_ascii_case(me))
}

/// The authenticated forge user, if it can be read.
fn viewer_login(forge: &dyn Forge, repo_root: &Path) -> Option<String> {
    let loc = GitLoc::from_db(&repo_root.to_string_lossy(), None);
    forge
        .whoami(&loc)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn describe(e: &ForgeError) -> String {
    e.describe()
}

/// Compose the prompt and run the agent in the PR's worktree.
fn run_agent(
    cfg: &PrQueueConfig,
    kind: TaskKind,
    template: &str,
    worktree: &str,
    item: &PrItem,
    fetched: &FetchedPr,
    blocker: &Blocker,
) {
    let Some((vars, prompt)) = compose(cfg, kind, worktree, item, fetched, blocker) else {
        return;
    };
    crate::agent_run::run(&crate::agent_run::AgentTaskRun {
        kind,
        worktree,
        prompt: &prompt,
        command_template: template,
        vars: &vars,
        timeout_secs: cfg.agent_timeout_secs,
    });
}

/// Build the task variables and render the prompt for a PR blocker.
///
/// Split out of [`run_agent`] so the mapping (which blocker fills which
/// variables) is unit-testable without spawning anything.
fn compose(
    cfg: &PrQueueConfig,
    kind: TaskKind,
    worktree: &str,
    item: &PrItem,
    fetched: &FetchedPr,
    blocker: &Blocker,
) -> Option<(TaskVars, String)> {
    let pr = &fetched.pr;
    let mut vars = TaskVars::new()
        .set("branch", &item.branch)
        .set("base", &pr.base_ref_name)
        .set("worktree", worktree)
        .set("pr_number", item.number.to_string())
        .set("pr_url", &pr.url)
        .set("pr_title", &pr.title);

    match kind {
        TaskKind::PrCiFailure => {
            let names = match blocker {
                Blocker::Ci(n) => n.join(", "),
                _ => String::new(),
            };
            vars = vars
                .set("checks", names)
                // The forge's rollup carries no log text; point the agent at the
                // run instead of inventing output it can't see.
                .set("log", check_urls(fetched));
        }
        TaskKind::PrReview => {
            vars = vars.set("threads", format_threads(&fetched.threads));
        }
        // The conflict prompt needs only the identity vars set above.
        _ => {}
    }

    match thegn_core::agent_task::render_prompt(cfg.prompts.resolve(kind), &vars) {
        Ok(p) => Some((vars, p)),
        Err(e) => {
            tracing::warn!(
                target: "thegn::prq",
                kind = %kind,
                error = %e,
                "pr queue: prompt template is invalid; not dispatching"
            );
            None
        }
    }
}

/// Where the agent can read the failing runs. Bounded so a repo with dozens of
/// checks cannot bloat the prompt.
fn check_urls(f: &FetchedPr) -> String {
    use thegn_core::forge::model::{Bucket, check_bucket};
    let mut out = String::new();
    for c in
        f.pr.status_check_rollup
            .iter()
            .filter(|c| check_bucket(c) == Bucket::Fail)
            .take(5)
    {
        let name = if c.name.trim().is_empty() {
            "check"
        } else {
            &c.name
        };
        match c.details_url.as_deref() {
            Some(u) if !u.is_empty() => out.push_str(&format!("  - {name}: {u}\n")),
            _ => out.push_str(&format!("  - {name}\n")),
        }
    }
    if out.is_empty() {
        out.push_str("  (no failing check details available — inspect CI directly)\n");
    }
    out
}

/// Unresolved review threads, as prompt text. Bounded for the same reason.
fn format_threads(threads: &[ReviewThreadRow]) -> String {
    let mut out = String::new();
    for t in threads.iter().filter(|t| !t.resolved).take(20) {
        let where_ = match t.line {
            Some(l) if !t.path.is_empty() => format!("{}:{l}", t.path),
            _ if !t.path.is_empty() => t.path.clone(),
            _ => "(general)".to_string(),
        };
        out.push_str(&format!("  - {} on {where_}: {}\n", t.author, t.snippet));
    }
    if out.is_empty() {
        out.push_str("  (no unresolved threads found)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::forge::model::PrStatus;

    fn cfg() -> PrQueueConfig {
        PrQueueConfig {
            enabled: true,
            ..PrQueueConfig::default()
        }
    }

    fn item() -> PrItem {
        PrItem {
            key: "/repo#7".into(),
            number: 7,
            branch: "feat".into(),
            worktree: Some("/w/feat".into()),
            agent_attempts: 0,
            last_head_oid: Some("abc".into()),
            forge: "github".into(),
        }
    }

    fn thread(author: &str, path: &str, line: Option<u64>, resolved: bool) -> ReviewThreadRow {
        ReviewThreadRow {
            author: author.into(),
            path: path.into(),
            line,
            snippet: "please rename this".into(),
            resolved,
            created_at: String::new(),
        }
    }

    fn fetched(threads: Vec<ReviewThreadRow>) -> FetchedPr {
        FetchedPr {
            pr: PrStatus {
                number: 7,
                title: "Add widget".into(),
                state: "OPEN".into(),
                url: "https://github.com/me/repo/pull/7".into(),
                is_draft: false,
                head_ref_name: "feat".into(),
                head_ref_oid: "abc".into(),
                base_ref_name: "main".into(),
                mergeable: "MERGEABLE".into(),
                merge_state_status: "CLEAN".into(),
                review_decision: None,
                status_check_rollup: vec![],
                checks: Default::default(),
            },
            threads,
        }
    }

    #[test]
    fn the_conflict_prompt_names_the_pr_and_its_base() {
        let f = fetched(vec![]);
        let (vars, p) = compose(
            &cfg(),
            TaskKind::PrConflict,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Conflict,
        )
        .unwrap();
        assert_eq!(vars.get("pr_number"), Some("7"));
        assert_eq!(vars.get("base"), Some("main"));
        assert!(p.contains("#7") && p.contains("Add widget") && p.contains("main"));
        // The PR family's rules, not the merge queue's.
        assert!(p.contains("DO push") && p.contains("--force-with-lease"));
        assert!(p.contains("Do NOT merge"));
    }

    #[test]
    fn the_ci_prompt_lists_the_failing_checks() {
        let f = fetched(vec![]);
        let (vars, p) = compose(
            &cfg(),
            TaskKind::PrCiFailure,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Ci(vec!["clippy".into(), "test".into()]),
        )
        .unwrap();
        assert_eq!(vars.get("checks"), Some("clippy, test"));
        assert!(p.contains("clippy, test"));
        // With no rollup details, the prompt says so instead of implying output.
        assert!(p.contains("no failing check details"));
    }

    #[test]
    fn the_review_prompt_lists_only_unresolved_threads() {
        let f = fetched(vec![
            thread("alice", "src/a.rs", Some(12), false),
            thread("bob", "src/b.rs", Some(3), true),
        ]);
        let (_, p) = compose(
            &cfg(),
            TaskKind::PrReview,
            "/w/feat",
            &item(),
            &f,
            &Blocker::ChangesRequested,
        )
        .unwrap();
        assert!(p.contains("alice") && p.contains("src/a.rs:12"));
        assert!(
            !p.contains("bob"),
            "a resolved thread is not outstanding work"
        );
        // And the agent is told to leave resolution to the reviewer.
        assert!(p.contains("Do NOT resolve"));
    }

    #[test]
    fn a_review_with_nothing_unresolved_says_so_rather_than_going_blank() {
        let f = fetched(vec![thread("bob", "x", None, true)]);
        let (_, p) = compose(
            &cfg(),
            TaskKind::PrReview,
            "/w/feat",
            &item(),
            &f,
            &Blocker::ChangesRequested,
        )
        .unwrap();
        assert!(p.contains("no unresolved threads"));
    }

    #[test]
    fn a_custom_template_replaces_the_builtin() {
        let mut c = cfg();
        c.prompts.conflict = "rebase {branch} onto {base} for #{pr_number}".into();
        let f = fetched(vec![]);
        let (_, p) = compose(
            &c,
            TaskKind::PrConflict,
            "/w/feat",
            &item(),
            &f,
            &Blocker::Conflict,
        )
        .unwrap();
        assert_eq!(p, "rebase feat onto main for #7");
    }

    #[test]
    fn a_broken_template_refuses_to_dispatch() {
        let mut c = cfg();
        c.prompts.review = "{nope}".into();
        let f = fetched(vec![]);
        assert!(
            compose(
                &c,
                TaskKind::PrReview,
                "/w/feat",
                &item(),
                &f,
                &Blocker::ChangesRequested
            )
            .is_none(),
            "a typo'd placeholder must not reach the agent as a blank"
        );
    }

    #[test]
    fn authorship_is_unknown_unless_the_viewer_is_known() {
        let f = fetched(vec![]);
        // Unknown viewer ⇒ not ours, so `own_prs_only` errs toward NOT writing.
        assert!(!is_own_pr(&f, None));
        assert!(!is_own_pr(&f, Some("")));
        assert!(is_own_pr(&f, Some("me")));
        assert!(is_own_pr(&f, Some("ME")), "case-insensitive");
        assert!(!is_own_pr(&f, Some("someone-else")));
    }

    #[test]
    fn blocker_details_are_human_readable() {
        let f = fetched(vec![
            thread("a", "x", None, false),
            thread("b", "y", None, false),
        ]);
        assert!(blocker_detail(&Blocker::Ci(vec!["t".into()]), &f).contains("failing: t"));
        assert!(blocker_detail(&Blocker::Conflict, &f).contains("main"));
        assert!(blocker_detail(&Blocker::ChangesRequested, &f).contains("2 unresolved"));
        assert!(blocker_detail(&Blocker::Draft, &f).contains("draft"));
        assert!(blocker_detail(&Blocker::None, &f).contains("mergeable"));
    }

    #[test]
    fn a_failing_row_backs_off_and_a_success_clears_it() {
        // Distinct key per test run: the registry is process-global.
        let key = "/repo#backoff-test";
        let now = 1_000i64;
        assert!(!backoff_active(key, now), "clean row is never in backoff");

        let w1 = record_failure(key, now, 60);
        assert!(w1 > 0, "a failure must arm a window");
        assert!(backoff_active(key, now), "armed window skips the row");
        assert!(
            !backoff_active(key, now + w1 as i64),
            "the window expires rather than sticking"
        );

        // Consecutive failures widen it, so a forge outage isn't re-hammered.
        let w2 = record_failure(key, now, 60);
        assert!(w2 > w1, "backoff must grow: {w1} → {w2}");

        record_success(key);
        assert!(!backoff_active(key, now), "a good fetch clears the window");
        // ...and the failure count resets, so the next blip starts small again.
        assert_eq!(record_failure(key, now, 60), w1);
        record_success(key);
    }

    #[test]
    fn merge_methods_map_across() {
        assert_eq!(merge_method(PrMergeMethod::Squash), MergeMethod::Squash);
        assert_eq!(merge_method(PrMergeMethod::Merge), MergeMethod::Merge);
        assert_eq!(merge_method(PrMergeMethod::Rebase), MergeMethod::Rebase);
    }

    #[test]
    fn check_urls_are_bounded_and_prefer_details_links() {
        use thegn_core::forge::model::CheckRun;
        let mk = |n: &str, url: Option<&str>| CheckRun {
            name: n.into(),
            status: "COMPLETED".into(),
            conclusion: Some("FAILURE".into()),
            state: None,
            workflow_name: None,
            details_url: url.map(String::from),
            started_at: None,
            completed_at: None,
        };
        let mut f = fetched(vec![]);
        f.pr.status_check_rollup = (0..9).map(|i| mk(&format!("c{i}"), Some("u"))).collect();
        let s = check_urls(&f);
        assert_eq!(s.lines().count(), 5, "bounded so the prompt can't bloat");
        assert!(s.contains("c0: u"));
    }

    // ---- the fake forge the trait was written for ----------------------------

    /// Records every call; answers `fetch` with a configurable PR.
    struct FakeForge {
        pr: std::sync::Mutex<Option<PrStatus>>,
        calls: std::sync::Mutex<Vec<String>>,
    }
    impl FakeForge {
        fn new(pr: Option<PrStatus>) -> Self {
            FakeForge {
                pr: std::sync::Mutex::new(pr),
                calls: Default::default(),
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn record(&self, s: impl Into<String>) {
            self.calls.lock().unwrap().push(s.into());
        }
    }
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
            "github"
        }
        fn caps(&self) -> thegn_core::forge::ForgeCaps {
            thegn_core::forge::ForgeCaps::ALL
        }
        fn repo_ref(&self, _: &GitLoc) -> Option<thegn_core::forge::RepoRef> {
            None
        }
        fn pr_status(&self, _: &GitLoc, pr: PrRef) -> Result<PrStatus, ForgeError> {
            self.record(format!("pr_status {pr:?}"));
            self.pr.lock().unwrap().clone().ok_or(ForgeError::NoPr)
        }
        fn pr_list(
            &self,
            _: &GitLoc,
            _: usize,
        ) -> Result<Vec<thegn_core::forge::model::PrHeader>, ForgeError> {
            Ok(vec![])
        }
        fn merge_pr(
            &self,
            _: &GitLoc,
            pr: PrRef,
            method: MergeMethod,
            delete_branch: bool,
            auto: bool,
        ) -> Result<(), ForgeError> {
            self.record(format!(
                "merge {pr:?} {method:?} del={delete_branch} auto={auto}"
            ));
            Ok(())
        }
        fn rerun_failed(&self, _: &GitLoc, pr: PrRef) -> Result<u32, ForgeError> {
            self.record(format!("rerun {pr:?}"));
            Ok(0)
        }
        fn whoami(&self, _: &GitLoc) -> Result<String, ForgeError> {
            self.record("whoami");
            Ok("me".into())
        }
    }

    fn green_pr() -> PrStatus {
        let mut pr = fetched(vec![]).pr;
        pr.review_decision = Some("APPROVED".into());
        pr
    }

    fn temp_db(name: &str) -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open_at(&dir.path().join(format!("{name}.db"))).unwrap();
        (dir, db)
    }

    #[test]
    fn fake_forge_drives_a_green_pr_to_auto_merge() {
        let (_dir, db) = temp_db("prq-auto");
        let forge = FakeForge::new(Some(green_pr()));
        let cfg = cfg(); // merge_mode = auto_merge (default)
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![item()],
            |_| {},
        );
        assert_eq!(out.merged, vec![7], "{out:?}");
        let calls = forge.calls();
        assert!(calls.iter().any(|c| c == "whoami"), "{calls:?}");
        assert!(
            calls.iter().any(|c| c.starts_with("pr_status Number(7)")),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with("merge Number(7)") && c.ends_with("auto=true")),
            "auto-merge is merge_pr with auto=true: {calls:?}"
        );
    }

    #[test]
    fn fake_forge_direct_mode_merges_now() {
        let (_dir, db) = temp_db("prq-direct");
        let forge = FakeForge::new(Some(green_pr()));
        let mut cfg = cfg();
        cfg.merge_mode = thegn_core::config::PrMergeMode::Thegn;
        let out = drive_queue(
            &cfg,
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![item()],
            |_| {},
        );
        assert_eq!(out.merged, vec![7], "{out:?}");
        assert!(
            forge
                .calls()
                .iter()
                .any(|c| c.starts_with("merge Number(7)") && c.ends_with("auto=false")),
            "{:?}",
            forge.calls()
        );
    }

    #[test]
    fn fake_forge_fetch_failure_is_a_warning_not_a_verdict() {
        let (_dir, db) = temp_db("prq-gone");
        let forge = FakeForge::new(None); // every fetch → NoPr
        let out = drive_queue(
            &cfg(),
            &Config::default(),
            &forge,
            Path::new("/repo"),
            &db,
            vec![item()],
            |_| {},
        );
        assert!(out.merged.is_empty());
        assert!(
            !forge.calls().iter().any(|c| c.starts_with("merge")),
            "never merges what it could not fetch: {:?}",
            forge.calls()
        );
    }
}
