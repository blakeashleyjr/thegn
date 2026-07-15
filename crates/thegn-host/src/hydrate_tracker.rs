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
pub(crate) fn spawn_issue_cache_refresh(
    cwd: std::path::PathBuf,
    cfg: thegn_core::config::IssuesConfig,
    waker: Option<TerminalWaker>,
) {
    crate::sched::spawn_bg(move || {
        use thegn_core::issue::IssueFilter;
        use thegn_svc::issue::IssueRouter;

        if !cwd.is_dir() {
            return;
        }
        let router = IssueRouter::from_config(&cfg);
        if !router.is_configured() {
            return;
        }
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        let filter = IssueFilter {
            assignee_me: cfg.filter_assignee_me,
            limit: cfg.max_issues,
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
            let Ok(issues) = result else {
                continue; // a failing account leaves its prior cache intact
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
            let old_map: std::collections::HashMap<&str, &thegn_core::issue::IssueStatus> =
                old_issues
                    .iter()
                    .map(|i| (i.id.as_str(), &i.status))
                    .collect();
            for issue in &issues {
                if let Some(&old_status) = old_map.get(issue.id.as_str())
                    && *old_status != issue.status
                    && linked.contains(&issue.id)
                {
                    let msg = format!(
                        "{} status changed to {}",
                        issue.number,
                        issue.status.label()
                    );
                    let _ = db.put_notification("status_changed", &issue.id, &msg, &repo_key);
                }
            }
            let _ = db.put_issue_cache(&repo_key, provider, &account, &json);
            changed = true;
        }
        if changed && let Some(w) = &waker {
            let _ = w.wake();
        }
    });
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
