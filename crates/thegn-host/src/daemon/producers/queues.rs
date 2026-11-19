//! The daemon's PR queue producer and autopilot pickup (THE-2208, THE-2211).
//!
//! - [`tick_pr_queue`] drives every repo with queued PR rows through the same
//!   blocking body the UI uses ([`drive_blocking`]) in
//!   [`DriveMode::Daemon`]: an agent handoff happens only when
//!   `[pr_queue] agent_sandbox` is on and the agent passes the shared-kernel
//!   (or stricter) floor with a fail-closed miss; otherwise the blocker is
//!   reported, never fixed (the notify degrade).
//! - [`daemon_pickup`] is the tracker tick's autopilot pickup: refused while
//!   the automation runtime is paused (fail-closed), otherwise the same
//!   hold → admit → permit → durable claim path as the UI entry, with the
//!   sealed autopilot sandbox. Admission
//!   (`autopilot_driver::admit_pickup`, THE-2196) applies the automation
//!   pause and budgets and writes the `autopilot:<repo>` run row.
//!
//! Both run on the producer owner's `spawn_blocking` worker: no new thread.
#![allow(
    dead_code,
    reason = "daemon_pickup is called by the tracker producer (b161 lane 6)"
)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use thegn_core::config::Config;
use thegn_core::db::Db;

use super::{TickCtx, TickOutcome};
use crate::handlers::pr_queue::{DriveMode, PrqMsg, drive_blocking};

/// At most this many repos are driven per PR queue tick.
pub(crate) const MAX_REPOS_PER_TICK: usize = 16;

/// Where the next tick's repo window starts, so repos past the cap rotate in.
static REPO_CURSOR: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// One PR queue tick: each repo with active queued PR rows (DB), capped at
/// [`MAX_REPOS_PER_TICK`], driven in [`DriveMode::Daemon`].
pub(crate) fn tick_pr_queue(ctx: &TickCtx) -> TickOutcome {
    if !ctx.cfg.pr_queue.enabled {
        return TickOutcome::Skipped("pr_queue disabled");
    }
    if automation_paused() {
        return TickOutcome::Skipped("automation paused");
    }
    let db = match Db::open() {
        Ok(db) => db,
        Err(error) => {
            tracing::warn!(target: "thegn::producers", %error, "pr queue tick: db unavailable");
            return TickOutcome::Skipped("db unavailable");
        }
    };
    let rows = match thegn_core::store::WorktreeAuxStore::list_pr_queue(&db) {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(target: "thegn::producers", %error, "pr queue tick: could not list rows");
            return TickOutcome::Skipped("pr queue rows unreadable");
        }
    };
    let worktrees: std::collections::HashMap<&str, &str> = rows
        .iter()
        .map(|r| (r.key.as_str(), r.worktree.as_deref().unwrap_or_default()))
        .collect();
    let (repos, next) = queued_repos(
        rows.iter()
            .map(|r| (r.repo_root.as_str(), r.status.as_str())),
        REPO_CURSOR.load(std::sync::atomic::Ordering::Relaxed),
    );
    REPO_CURSOR.store(next, std::sync::atomic::Ordering::Relaxed);
    if repos.is_empty() {
        return TickOutcome::Skipped("no queued PR rows");
    }
    for root in &repos {
        drive_blocking(&ctx.cfg, root, DriveMode::Daemon, |msg| match msg {
            PrqMsg::Failed(reason) => tracing::warn!(
                target: "thegn::producers",
                repo = %root.display(),
                %reason,
                "daemon pr queue pass failed"
            ),
            PrqMsg::Step {
                key,
                number,
                branch,
                status,
                detail,
            } => {
                let wt = worktrees.get(key.as_str()).copied().unwrap_or_default();
                crate::handlers::pr_queue::record_settled_step(
                    &db, &key, wt, number, &branch, &status, &detail,
                );
            }
            PrqMsg::Done(out) => {
                for warning in &out.warnings {
                    tracing::warn!(target: "thegn::producers", repo = %root.display(), %warning, "daemon pr queue pass");
                }
            }
            PrqMsg::Note(_) | PrqMsg::ReviewTask { .. } => {}
        });
    }
    TickOutcome::Done { work: repos.len() }
}

/// The distinct repo roots with at least one row the driver would still
/// work on (not merged, closed or needs_human), sorted, capped at
/// [`MAX_REPOS_PER_TICK`] starting at `cursor` (wrapping), plus the cursor for
/// the next tick so every repo is eventually driven.
fn queued_repos<'a>(
    rows: impl IntoIterator<Item = (&'a str, &'a str)>,
    cursor: usize,
) -> (Vec<PathBuf>, usize) {
    let set: BTreeSet<&str> = rows
        .into_iter()
        .filter(|(root, status)| {
            !root.trim().is_empty() && !matches!(*status, "merged" | "closed" | "needs_human")
        })
        .map(|(root, _)| root)
        .collect();
    let all: Vec<&str> = set.into_iter().collect();
    if all.is_empty() {
        return (Vec::new(), 0);
    }
    let start = cursor % all.len();
    let take = MAX_REPOS_PER_TICK.min(all.len());
    (
        (0..take)
            .map(|i| PathBuf::from(all[(start + i) % all.len()]))
            .collect(),
        (start + take) % all.len(),
    )
}

/// One autopilot pickup the tracker tick hands over.
pub(crate) struct PickupRequest<'a> {
    pub repo_root: &'a Path,
    pub cwd: &'a Path,
    pub account: &'a str,
    pub provider: &'a str,
    pub issues: &'a [thegn_core::issue::Issue],
    pub from_assignee_me: bool,
}

/// Autopilot pickup inside the daemon. Paused automation (or an unreadable
/// pause flag) picks nothing up.
pub(crate) fn daemon_pickup(cfg: &Config, req: &PickupRequest<'_>) {
    daemon_pickup_with(automation_paused(), cfg, req, |cfg, req| {
        crate::autopilot_driver::pickup_from_daemon(
            cfg,
            req.repo_root,
            req.cwd,
            req.account,
            req.provider,
            req.issues,
            req.from_assignee_me,
        );
    });
}

/// The pause decision around a pickup, with the pickup injected. Returns
/// whether the pickup ran.
fn daemon_pickup_with(
    paused: bool,
    cfg: &Config,
    req: &PickupRequest<'_>,
    pickup: impl FnOnce(&Config, &PickupRequest<'_>),
) -> bool {
    if paused {
        tracing::debug!(
            target: "thegn::autopilot",
            repo = %req.repo_root.display(),
            "daemon autopilot pickup skipped: automation paused"
        );
        return false;
    }
    pickup(cfg, req);
    true
}

/// The automation pause flag, fail-closed: a DB that cannot be opened or read
/// counts as paused.
fn automation_paused() -> bool {
    match Db::open() {
        Ok(db) => thegn_core::automation_control::paused_fail_closed(&db),
        Err(error) => {
            tracing::warn!(
                target: "thegn::autopilot",
                %error,
                "daemon autopilot pickup: db unavailable; treating automation as paused"
            );
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request<'a>(root: &'a Path, issues: &'a [thegn_core::issue::Issue]) -> PickupRequest<'a> {
        PickupRequest {
            repo_root: root,
            cwd: root,
            account: "acct",
            provider: "linear",
            issues,
            from_assignee_me: true,
        }
    }

    #[test]
    fn daemon_pickup_paused_picks_nothing_up() {
        let root = Path::new("/nonexistent/repo");
        let req = request(root, &[]);
        let mut ran = false;
        assert!(!daemon_pickup_with(
            true,
            &Config::default(),
            &req,
            |_, _| ran = true
        ));
        assert!(!ran, "a paused runtime must not reach the pickup");
        assert!(daemon_pickup_with(
            false,
            &Config::default(),
            &req,
            |_, _| ran = true
        ));
        assert!(ran);
    }

    #[test]
    fn daemon_pickup_reads_the_pause_flag_fail_closed() {
        let _state = crate::testenv::IsolatedState::new();
        let db = Db::open().unwrap();
        assert!(!automation_paused());
        thegn_core::automation_control::set_paused(&db, true).unwrap();
        assert!(automation_paused());
        thegn_core::automation_control::set_paused(&db, false).unwrap();
        assert!(!automation_paused());
    }

    #[test]
    fn daemon_pickup_checks_pause_before_the_shared_pickup() {
        let source = include_str!("queues.rs");
        let production = source.split_once("\n#[cfg(test)]\nmod tests {").unwrap().0;
        let start = production.find("pub(crate) fn daemon_pickup(").unwrap();
        let end = production[start..].find("\n}\n").unwrap() + start;
        let body = &production[start..end];
        let paused = body.find(concat!("automation_", "paused()")).unwrap();
        let pickup = body.find(concat!("pickup_", "from_daemon(")).unwrap();
        assert!(paused < pickup);
    }

    #[test]
    fn pr_queue_tick_selects_distinct_active_repos_capped() {
        let rows = [
            ("/r/b", "blocked_ci"),
            ("/r/a", "watching"),
            ("/r/a", "ready"),
            ("/r/c", "merged"),
            ("/r/d", "closed"),
            ("/r/e", "needs_human"),
            ("", "watching"),
        ];
        assert_eq!(
            queued_repos(rows, 0).0,
            vec![PathBuf::from("/r/a"), PathBuf::from("/r/b")]
        );

        let many: Vec<String> = (0..40).map(|i| format!("/r/{i:02}")).collect();
        let (picked, next) = queued_repos(many.iter().map(|r| (r.as_str(), "watching")), 0);
        assert_eq!(picked.len(), MAX_REPOS_PER_TICK);
        assert_eq!(picked[0], PathBuf::from("/r/00"));
        assert_eq!(next, 16);

        let (picked, _) = queued_repos(many.iter().map(|r| (r.as_str(), "watching")), next);
        assert_eq!(picked[0], PathBuf::from("/r/16"));

        let (picked, _) = queued_repos(many.iter().map(|r| (r.as_str(), "watching")), 32);
        assert!(picked.contains(&PathBuf::from("/r/39")));
        assert!(picked.contains(&PathBuf::from("/r/00")));
    }

    #[test]
    fn pr_queue_tick_skips_while_paused() {
        let _state = crate::testenv::IsolatedState::new();
        thegn_core::automation_control::set_paused(&Db::open().unwrap(), true).unwrap();
        let mut cfg = Config::default();
        cfg.pr_queue.enabled = true;
        let ctx = TickCtx {
            cfg: std::sync::Arc::new(cfg),
            now_ms: 0,
        };
        assert_eq!(
            tick_pr_queue(&ctx),
            TickOutcome::Skipped("automation paused")
        );
    }

    #[test]
    fn pr_queue_tick_with_no_rows_does_nothing() {
        let _state = crate::testenv::IsolatedState::new();
        let mut cfg = Config::default();
        cfg.pr_queue.enabled = true;
        let ctx = TickCtx {
            cfg: std::sync::Arc::new(cfg),
            now_ms: 0,
        };
        assert_eq!(
            tick_pr_queue(&ctx),
            TickOutcome::Skipped("no queued PR rows")
        );
    }

    #[test]
    fn pr_queue_tick_is_inert_when_disabled() {
        let ctx = TickCtx {
            cfg: std::sync::Arc::new(Config::default()),
            now_ms: 0,
        };
        assert_eq!(
            tick_pr_queue(&ctx),
            TickOutcome::Skipped("pr_queue disabled")
        );
    }
}
