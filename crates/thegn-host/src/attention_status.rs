//! Per-worktree attention scoring on the hydration thread.
//!
//! Joins the signal sources the app already maintains — the activity FSM
//! snapshot, unread notifications, the PR / CI caches, and the merge queue —
//! into one [`thegn_core::attention::AttentionScore`] per worktree path,
//! plus a **hysteresis-stable** display order for the sidebar's Attention sort
//! (see [`thegn_core::attention::stable_order`]: only a tier or membership
//! change reorders; timestamp/cache churn never does). Runs at the end of
//! `collect_sidebar_status`, so it is off-loop and repaint-gated by the
//! status diff like every other sidebar signal.
//!
//! Staleness caveats, accepted for v1: the PR/CI caches are refreshed for the
//! *active* worktree only, so PR-derived tiers for background worktrees are
//! last-known-good; and the mid-creation `Loading` overlay is loop-side state
//! the hydration thread can't see (those rows briefly score as idle).

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

use thegn_core::attention::{
    self, ActivityKind, AttentionInputs, AttentionScore, AttentionTier, MqFacts, MqStatus, PrFacts,
    UnreadNote,
};
use thegn_core::notification::NotificationKind;
use thegn_core::store::{CacheStore, NotificationStore, WorkspaceStore, WorktreeAuxStore};

/// The previous hysteresis-stable order `(path, tier)` — the `glyph_cache()`
/// pattern: process-global so it survives across hydration passes.
fn order_memo() -> &'static Mutex<Vec<(String, AttentionTier)>> {
    static MEMO: OnceLock<Mutex<Vec<(String, AttentionTier)>>> = OnceLock::new();
    MEMO.get_or_init(|| Mutex::new(Vec::new()))
}

/// Compute `status.attention` / `attention_ranks` / `workspace_attention` for
/// every registered + live worktree. All inputs are cheap DB/snapshot reads;
/// the branching lives in `thegn_core::attention::score`.
pub(crate) fn collect_attention(
    session: &crate::session::Session,
    db: &thegn_core::db::Db,
    status: &mut crate::sidebar::SidebarStatus,
) {
    // Worktree universe: registered rows, overlaid with live session groups
    // (which may be unpersisted). `(is_home, position)` is the fresh-sort
    // tie-break inside a tier.
    struct Meta {
        slug: String,
        is_home: bool,
        position: i64,
    }
    let mut meta: BTreeMap<String, Meta> = BTreeMap::new();
    for wt in db.worktrees().unwrap_or_default() {
        if wt.worktree.is_empty() {
            continue;
        }
        let slug = wt
            .tab_name
            .split_once('/')
            .map(|(s, _)| s.to_string())
            .unwrap_or_default();
        meta.insert(
            wt.worktree.clone(),
            Meta {
                slug,
                is_home: wt.branch == "home",
                position: wt.position,
            },
        );
    }
    for (gi, g) in session.worktrees.iter().enumerate() {
        if g.path.is_empty() {
            continue;
        }
        let (slug, branch) = crate::sidebar::split_tab(&g.name).unwrap_or_default();
        meta.entry(g.path.clone()).or_insert(Meta {
            slug,
            is_home: branch == "home",
            position: gi as i64,
        });
    }
    if meta.is_empty() {
        return;
    }

    // Activity FSM snapshot, path-keyed with real state timestamps.
    let activity = thegn_core::activity::read_entries();

    // Unread notifications grouped by worktree (host-global rows have an empty
    // path and never mark a worktree).
    let mut unread: BTreeMap<String, Vec<UnreadNote>> = BTreeMap::new();
    for n in db.get_unread_notifications().unwrap_or_default() {
        if n.worktree_path.is_empty() {
            continue;
        }
        unread.entry(n.worktree_path).or_default().push(UnreadNote {
            kind: n.kind,
            at: n.created_at_ms, // unix seconds despite the legacy name
        });
    }

    // Last-known-good PR facts per worktree, one table read.
    let mut pr: BTreeMap<String, PrFacts> = BTreeMap::new();
    for (worktree, json, _fetched_at) in db.list_pr_cache().unwrap_or_default() {
        if let Ok(mut st) = serde_json::from_str::<thegn_core::github::PrStatus>(&json) {
            st.recompute_checks(); // `checks` is skip_deserializing
            if let Some(facts) = PrFacts::from_status(&st) {
                pr.insert(worktree, facts);
            }
        }
    }

    // Merge-queue entries (one row per worktree; `landed` scores as no signal).
    let mut mq: BTreeMap<String, MqFacts> = BTreeMap::new();
    for row in db.list_merge_queue().unwrap_or_default() {
        if let Some(st) = MqStatus::parse(&row.status) {
            mq.insert(
                row.worktree,
                MqFacts {
                    status: st,
                    updated_at: row.updated_at,
                },
            );
        }
    }
    // Re-expose the raw statuses for the sidebar's per-worktree MQ chip (the
    // scorer folds them into tiers; the chip wants the status itself).
    status.mq = mq.iter().map(|(p, f)| (p.clone(), f.status)).collect();

    // Score every worktree.
    let mut scores: BTreeMap<String, AttentionScore> = BTreeMap::new();
    for path in meta.keys() {
        let act = activity.get(path);
        let (activity_kind, activity_since) = match act.map(|e| e.state.as_str()) {
            Some("active") => (
                ActivityKind::Active,
                act.and_then(|e| e.busy_since).map(|s| s as i64),
            ),
            Some("waiting") => (
                ActivityKind::Waiting,
                act.and_then(|e| e.quiet_since).map(|s| s as i64),
            ),
            Some("read") => (
                ActivityKind::Read,
                act.and_then(|e| e.quiet_since).map(|s| s as i64),
            ),
            _ => (ActivityKind::None, None),
        };
        // Latest cached CI run (newest first in the cache), last-known-good.
        let (mut ci_failing, mut ci_running) = (false, false);
        if let Ok(Some((json, _))) = db.get_ci_cache(path)
            && let Ok(runs) = serde_json::from_str::<Vec<thegn_core::ci::CiRun>>(&json)
            && let Some(latest) = runs.first()
        {
            ci_failing = latest.state.is_failure();
            ci_running = matches!(
                latest.state,
                thegn_core::ci::CiState::Running | thegn_core::ci::CiState::Pending
            );
        }
        // A real agent is bound iff `status.agent` has a non-shell entry: the
        // map is already tool-filtered in `hydrate` (yazi/lazygit/… skipped via
        // `tool_command`), so only the `"shell"`/`"local"` default sentinels
        // remain to exclude here.
        let has_agent = status
            .agent
            .get(path)
            .is_some_and(|a| !a.is_empty() && a != "shell" && a != "local");
        let inputs = AttentionInputs {
            activity: activity_kind,
            activity_since,
            unread: unread.remove(path).unwrap_or_default(),
            pr: pr.get(path).copied(),
            ci_failing,
            ci_running,
            merge_queue: mq.get(path).copied(),
            dirty: status.git.get(path).is_some_and(|g| g.dirty),
            has_agent,
        };
        scores.insert(path.clone(), attention::score(&inputs));
    }

    // Acknowledgements: an acked worktree is suppressed from the nag surfaces
    // (badge + "Needs you" popup) only while its *current* score still matches
    // the acked `(reason, since)`. A changed reason / advanced `since` (a new
    // episode) no longer matches — so we drop the now-stale ack (best-effort;
    // the DB is a cache) and the item re-nags.
    status.acked.clear();
    for (path, reason_str, since) in db.list_attention_acks().unwrap_or_default() {
        let Ok(reason) =
            serde_json::from_str::<thegn_core::attention::AttentionReason>(&reason_str)
        else {
            let _ = db.delete_attention_ack(&path); // best-effort: unparseable → prune
            continue;
        };
        let ack = thegn_core::attention::AttentionAck { reason, since };
        match scores.get(&path) {
            Some(s) if s.is_acked_by(&ack) => {
                status.acked.insert(path);
            }
            // Reason/episode changed, or the worktree scores nothing now: the
            // ack is stale. Prune so a genuinely new signal re-fires.
            _ => {
                let _ = db.delete_attention_ack(&path); // best-effort: DB is a cache
            }
        }
    }

    // Fresh order: urgency, then home-first / persisted position / path within
    // equal urgency — then hysteresis against the previous order.
    let mut fresh: Vec<(String, AttentionScore)> =
        scores.iter().map(|(p, s)| (p.clone(), *s)).collect();
    fresh.sort_by(|(pa, sa), (pb, sb)| {
        let ma = &meta[pa];
        let mb = &meta[pb];
        (sa.sort_key(), !ma.is_home, ma.position, pa).cmp(&(
            sb.sort_key(),
            !mb.is_home,
            mb.position,
            pb,
        ))
    });
    let order = {
        let mut memo = order_memo().lock().unwrap();
        let order = attention::stable_order(&memo, &fresh);
        *memo = order.iter().map(|p| (p.clone(), scores[p].tier)).collect();
        order
    };
    status.attention_ranks = order
        .iter()
        .enumerate()
        .map(|(i, p)| (p.clone(), i as u32))
        .collect();

    // Workspace rollups: each slug's most urgent worktree.
    let mut by_slug: BTreeMap<String, Vec<AttentionScore>> = BTreeMap::new();
    for (path, score) in &scores {
        let slug = &meta[path].slug;
        if !slug.is_empty() {
            by_slug.entry(slug.clone()).or_default().push(*score);
        }
    }
    status.workspace_attention = by_slug
        .into_iter()
        .filter_map(|(slug, ss)| attention::rollup(ss.iter()).map(|r| (slug, r)))
        .collect();
    status.attention = scores;
}

/// The attention-relevant notification kinds — documents the mapping the core
/// scorer applies (everything else is ambient and never raises a tier).
#[allow(dead_code)]
pub(crate) const SCORED_KINDS: [NotificationKind; 6] = [
    NotificationKind::AgentAttention,
    NotificationKind::AgentFailed,
    NotificationKind::TestFailed,
    NotificationKind::ProcessFailed,
    NotificationKind::LogError,
    NotificationKind::AgentDone,
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{GroupKind, Session, WorktreeGroup};
    use thegn_core::store::WorkspaceStore;

    fn session_with(paths: &[(&str, &str)]) -> Session {
        Session {
            id: "s".into(),
            worktrees: paths
                .iter()
                .map(|(name, path)| WorktreeGroup::new(*name, GroupKind::Home, *path))
                .collect(),
            active: 0,
        }
    }

    /// End-to-end over an in-memory DB: notifications / merge queue / PR cache
    /// raise the right tiers, ranks order by urgency, and rollups aggregate.
    #[test]
    fn scores_ranks_and_rollups_from_db_signals() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        // Three registered worktrees in one workspace, one in another.
        for (path, tab) in [
            ("/wt/idle", "app/idle"),
            ("/wt/blocked", "app/blocked"),
            ("/wt/failed", "app/failed"),
            ("/wt/other", "other/feat"),
        ] {
            let branch = tab.split('/').nth(1).unwrap();
            db.put_worktree(tab, "/repo", path, branch, None, None)
                .unwrap();
        }
        db.put_notification("agent_attention", "x", "needs you", "/wt/blocked")
            .unwrap();
        db.put_notification("test_failed", "y", "3 failed", "/wt/failed")
            .unwrap();
        // Host-global notifications never mark a worktree.
        db.put_notification("log_error", "log:thegn", "boom", "")
            .unwrap();

        let session = session_with(&[("app/idle", "/wt/idle")]);
        let mut status = crate::sidebar::SidebarStatus::default();
        // Reset the process-global memo so parallel tests can't leak an order in.
        order_memo().lock().unwrap().clear();
        collect_attention(&session, &db, &mut status);

        use thegn_core::attention::AttentionTier as T;
        assert_eq!(status.attention["/wt/blocked"].tier, T::Blocked);
        assert_eq!(status.attention["/wt/failed"].tier, T::Failure);
        assert_eq!(status.attention["/wt/idle"].tier, T::Idle);

        // Ranks: blocked < failed < idle.
        let r = &status.attention_ranks;
        assert!(r["/wt/blocked"] < r["/wt/failed"]);
        assert!(r["/wt/failed"] < r["/wt/idle"]);

        // Workspace rollup takes the most urgent child.
        assert_eq!(status.workspace_attention["app"].tier, T::Blocked);

        // Hysteresis: a second pass with unchanged tiers keeps the order.
        let ranks_before = status.attention_ranks.clone();
        let mut status2 = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut status2);
        assert_eq!(status2.attention_ranks, ranks_before);
    }

    #[test]
    fn ack_suppresses_matching_score_and_gcs_stale() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree("app/f", "/repo", "/wt/f", "f", None, None)
            .unwrap();
        db.put_notification("test_failed", "y", "3 failed", "/wt/f")
            .unwrap();
        let session = session_with(&[("app/f", "/wt/f")]);
        let mut status = crate::sidebar::SidebarStatus::default();
        order_memo().lock().unwrap().clear();
        collect_attention(&session, &db, &mut status);
        let sc = status.attention["/wt/f"];
        assert!(sc.needs_user());
        assert!(status.acked.is_empty(), "no acks yet");

        // Ack the exact showing (reason, since) → suppressed next pass.
        let reason = serde_json::to_string(&sc.reason).unwrap();
        db.put_attention_ack("/wt/f", &reason, sc.since).unwrap();
        let mut status2 = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut status2);
        assert!(status2.acked.contains("/wt/f"), "matching ack suppresses");

        // A stale ack (advanced `since` = new episode) is GC'd and re-nags.
        db.put_attention_ack("/wt/f", &reason, Some(sc.since.unwrap_or(0) + 1))
            .unwrap();
        let mut status3 = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut status3);
        assert!(
            !status3.acked.contains("/wt/f"),
            "stale ack does not suppress"
        );
        assert!(
            db.list_attention_acks().unwrap().is_empty(),
            "stale ack garbage-collected"
        );
    }

    #[test]
    fn merge_queue_row_scores_when_parseable() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree("app/q", "/repo", "/wt/q", "q", None, None)
            .unwrap();
        // Insert a queue row via the aux store.
        db.enqueue_merge("/wt/q", "q", "main").unwrap();
        db.update_merge_status("/wt/q", "needs_human", None, None, Some("conflict"))
            .unwrap();
        let session = session_with(&[("app/q", "/wt/q")]);
        let mut status = crate::sidebar::SidebarStatus::default();
        order_memo().lock().unwrap().clear();
        collect_attention(&session, &db, &mut status);
        assert_eq!(
            status.attention["/wt/q"].tier,
            thegn_core::attention::AttentionTier::Blocked
        );
        assert!(status.attention["/wt/q"].needs_user());
    }
}
