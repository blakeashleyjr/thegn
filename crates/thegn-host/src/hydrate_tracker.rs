//! Tracker (issue) hydration: the off-loop issue-cache refresh and the
//! panel-side cache loads. Extracted from the ratchet-pinned `hydrate.rs`
//! (mirroring the `hydrate_feed.rs` split).
//!
//! Threading contract: `spawn_issue_cache_refresh` runs its network + DB work
//! on `sched::spawn_bg` and pulses the waker once at the end;
//! `populate_tracker` runs on the hydration thread and reads only the DB
//! cache (no network).

use termwiz::terminal::TerminalWaker;
use thegn_core::store::{CacheStore, NotificationStore, WorktreeAuxStore};

/// Refresh the per-repo issue cache off-thread: fetch every configured
/// provider, diff old vs new per `(repo_root, provider)` key for
/// status-change notifications on linked issues, and rewrite the cache. A
/// failing provider leaves its prior cache intact.
///
/// Takes the full app `Config` so the fetch honors the repo's own `[issues]`
/// overlay (`Config::repo_issues`), and scopes GitHub to the active repo via
/// `--repo owner/repo` — without that, `gh issue list` resolves against the
/// **process cwd's** repo and every workspace's Issues section shows that one
/// repo's issues.
pub(crate) fn spawn_issue_cache_refresh(
    cwd: std::path::PathBuf,
    app_cfg: thegn_core::config::Config,
    waker: Option<TerminalWaker>,
) {
    crate::sched::spawn_bg(move || {
        use thegn_core::issue::IssueFilter;
        use thegn_svc::issue::IssueRouter;

        if !cwd.is_dir() {
            return;
        }
        let repo_root = thegn_core::repo::main_worktree(&cwd).unwrap_or_else(|| cwd.clone());
        let cfg = app_cfg.repo_issues(Some(&repo_root));
        let mut router = IssueRouter::from_config_at(&cfg, Some(&cwd));
        // Provider-as-plugin: append live plugin issue providers.
        crate::plugin_providers::extend_issue_router(&mut router);
        if !router.is_configured() {
            return;
        }
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        // Repo scope for GitHub (other providers scope by team/project and
        // ignore `repo`). Mirrors `spawn_my_work_refresh`.
        let loc = thegn_core::remote::GitLoc::for_worktree(&cwd);
        let filter = IssueFilter {
            assignee_me: cfg.filter_assignee_me,
            limit: cfg.max_issues,
            repo: crate::forge_handle::get()
                .for_loc(&loc)
                .repo_ref(&loc)
                .map(|r| r.nwo()),
            ..Default::default()
        };
        // Fetch every configured account; cache and diff each under its own
        // `(repo_root, provider, account)` key so trackers (and multiple
        // accounts of one provider) aggregate without clobbering.
        let per_provider = rt.block_on(router.list_per_provider(&filter));
        let Ok(db) = thegn_core::db::Db::open() else {
            return;
        };
        let repo_key = cwd.to_string_lossy();
        let linked: std::collections::HashSet<String> = db
            .linked_issues(&repo_key)
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut changed = false;
        for (account, provider, result) in per_provider {
            let issues = match result {
                Ok(issues) => {
                    // Reached the tracker — online evidence for the app-wide holder.
                    thegn_core::connectivity::report_success();
                    issues
                }
                Err(e) => {
                    // Only a dropped link is offline evidence (not a bad token).
                    if e.is_transient() {
                        thegn_core::connectivity::report_failure();
                    }
                    continue; // a failing account leaves its prior cache intact
                }
            };
            let Ok(json) = serde_json::to_string(&issues) else {
                continue;
            };
            // Diff old vs new for this account to emit notifications first.
            let old_issues: Vec<thegn_core::issue::Issue> = db
                .get_issue_cache(&repo_key, provider, &account)
                .ok()
                .flatten()
                .and_then(|(j, _)| serde_json::from_str(&j).ok())
                .unwrap_or_default();
            for (kind, source_ref, msg) in tracker_diff_notifications(&old_issues, &issues, &linked)
            {
                let _ = db.put_notification(kind, &source_ref, &msg, &repo_key);
            }
            // Overdue is re-derived (not diffed) each refresh; the store-side
            // emit-once (`put_notification_once`) keeps it to one row per
            // (issue, due date).
            for (source_ref, msg) in
                overdue_notifications(&issues, &linked, thegn_core::util::now())
            {
                let _ = db.put_notification_once("overdue", &source_ref, &msg, &repo_key);
            }
            let _ = db.put_issue_cache(&repo_key, provider, &account, &json);
            changed = true;
        }
        if changed && let Some(w) = &waker {
            let _ = w.wake();
        }
    });
}

/// The pure old-vs-new cache diff behind the tracker refresh's notification
/// emits: `(kind, source_ref, message)` rows for
/// - **status_changed** — a LINKED issue's status changed;
/// - **blocker_resolved** — a blocker of a LINKED issue flipped to Done this
///   refresh (the blocker itself need not be linked). Requires the blocker to
///   have been present before at a non-Done status, so a first fetch (empty
///   old cache) can never spray stale "resolved" notifications.
///
/// Pure so the emit-once semantics are unit-testable without a DB or network.
pub(crate) fn tracker_diff_notifications(
    old: &[thegn_core::issue::Issue],
    new: &[thegn_core::issue::Issue],
    linked: &std::collections::HashSet<String>,
) -> Vec<(&'static str, String, String)> {
    use thegn_core::issue::IssueStatus;
    let old_status: std::collections::HashMap<&str, IssueStatus> =
        old.iter().map(|i| (i.id.as_str(), i.status)).collect();
    let new_by_id: std::collections::HashMap<&str, &thegn_core::issue::Issue> =
        new.iter().map(|i| (i.id.as_str(), i)).collect();
    let mut out = Vec::new();
    for issue in new {
        if !linked.contains(&issue.id) {
            continue;
        }
        if let Some(&os) = old_status.get(issue.id.as_str())
            && os != issue.status
        {
            out.push((
                "status_changed",
                issue.id.clone(),
                format!(
                    "{} status changed to {}",
                    issue.number,
                    issue.status.label()
                ),
            ));
        }
        // A blocker done ⇒ this linked issue is unblocked. Attribute the
        // notification to the LINKED issue (that's whose worktree the user
        // cares about), naming the blocker in the message.
        for b_id in &issue.blocked_by {
            let Some(b_new) = new_by_id.get(b_id.as_str()) else {
                continue;
            };
            if b_new.status == IssueStatus::Done
                && old_status
                    .get(b_id.as_str())
                    .is_some_and(|&os| os != IssueStatus::Done)
            {
                out.push((
                    "blocker_resolved",
                    issue.id.clone(),
                    format!(
                        "{} unblocked — blocker {} is done",
                        issue.number, b_new.number
                    ),
                ));
            }
        }
    }
    out
}

/// Pure: `(source_ref, message)` rows for LINKED issues that are past their
/// due date and not Done/Cancelled. The message embeds the due date, so the
/// store's `(kind, ref, message)` emit-once dedupe fires once per (issue,
/// due date) — moving the date re-arms, a rerun with the same date doesn't.
pub(crate) fn overdue_notifications(
    issues: &[thegn_core::issue::Issue],
    linked: &std::collections::HashSet<String>,
    now_ms: i64,
) -> Vec<(String, String)> {
    use thegn_core::issue::IssueStatus;
    issues
        .iter()
        .filter(|i| linked.contains(&i.id))
        .filter(|i| !matches!(i.status, IssueStatus::Done | IssueStatus::Cancelled))
        .filter_map(|i| {
            let due = i.due_at_ms?;
            if due >= now_ms {
                return None;
            }
            let date = chrono::DateTime::from_timestamp_millis(due)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| due.to_string());
            Some((
                i.id.clone(),
                format!("{} overdue (was due {date})", i.number),
            ))
        })
        .collect()
}

/// Load the tracker caches into the panel model (hydration-thread side, DB
/// only — the background refresh keeps the cache warm). Loads every cached
/// provider for this repo and concatenates, so multiple trackers (e.g.
/// Linear + Jira) aggregate into one list.
pub(crate) fn populate_tracker(
    db: &thegn_core::db::Db,
    repo_key: &str,
    cwd: &std::path::Path,
    app_cfg: &thegn_core::config::Config,
    panel: &mut crate::panel::PanelData,
) {
    if let Ok(cached) = db.get_all_issue_cache(repo_key) {
        for (_provider, json) in cached {
            if let Ok(mut issues) = serde_json::from_str::<Vec<thegn_core::issue::Issue>>(&json) {
                panel.tracker_issues.append(&mut issues);
            }
        }
    }
    if let Ok(links) = db.linked_issues(&cwd.to_string_lossy()) {
        panel.tracker_links = links;
    }
    // Pure config check (no secrets, no network): is any issue account active
    // (explicit `[[issue_accounts]]` or a synthesized legacy provider)? Lets the
    // panel say "off" (unconfigured) vs "clear" (empty) honestly.
    panel.issues_configured = !app_cfg.issues.active_accounts().is_empty();
}
