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
        /// Owning repo root — the key the nag surfaces scope by. Empty when
        /// unresolvable.
        repo: String,
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
                repo: wt.repo_root.clone(),
            },
        );
    }
    for (gi, g) in session.worktrees.iter().enumerate() {
        if g.path.is_empty() {
            continue;
        }
        let (slug, branch) = crate::sidebar::split_tab(&g.name).unwrap_or_default();
        meta.entry(g.path.clone()).or_insert_with(|| Meta {
            slug,
            is_home: branch == "home",
            position: gi as i64,
            // Session-only groups (freshly created, not yet persisted) aren't in
            // the registry, so resolve their repo the same way the registry rows
            // carry it. One lookup per unpersisted group — usually zero.
            repo: db.repo_root_for(&g.path).ok().flatten().unwrap_or_default(),
        });
    }
    if meta.is_empty() {
        return;
    }

    // Nag scope: the worktrees of the *active* worktree's repo. The nag surfaces
    // (`✋` badge, "Needs you" popup, `Alt a` ring) default to this so a sibling
    // repo's failing CI can't nag you in the repo you're working in — matching
    // what the notification inbox already does (`hydrate_feed::populate_notifications`).
    //
    // `None` means "scope nothing", from either the "show everything" toggle or
    // an unresolvable active repo — the latter **fails open** deliberately, so a
    // scoping bug can never *hide* a signal that needs the user. Resolving the
    // toggle here, at the one place the scope is computed, keeps the render-time
    // predicate (`handlers::attention::in_scope`) pure over the model; `g`
    // rehydrates, so the flip still takes effect immediately.
    //
    // Note `status.attention` / `workspace_attention` stay GLOBAL below: the
    // sidebar renders every workspace's rows with their own tier glyph, and the
    // rollup needs every worktree. Scoping is a property of the nag, not of the
    // score.
    //
    // A terminal tab has no path, so it cannot name a repo: scope to the
    // session's (= workspace's) first worktree instead of failing open — a
    // terminal click used to flip the ✋ chip from `2 +3` to `5` and back.
    status.repo_scope = if crate::panel::scope::system_all() {
        None
    } else {
        session
            .active_group()
            .filter(|g| !g.path.is_empty())
            .or_else(|| session.worktrees.iter().find(|g| !g.path.is_empty()))
            .and_then(|g| meta.get(&g.path))
            .map(|m| m.repo.clone())
            .filter(|r| !r.is_empty())
            .map(|active_repo| {
                meta.iter()
                    .filter(|(_, m)| m.repo == active_repo)
                    .map(|(p, _)| p.clone())
                    .collect()
            })
    };

    // Activity FSM snapshot, path-keyed with real state timestamps.
    let activity = thegn_core::activity::read_entries();

    // Recency for the sidebar's `Live` sort: the last time each worktree was
    // busy (CPU or fresh output). Bucketed to 2s so sub-second poll jitter
    // between two co-active worktrees doesn't reorder them every hydration —
    // equal buckets fall through to the stable session-slot tie-break. Built
    // from the snapshot already in hand; no extra I/O, off the loop thread.
    status.activity_recency = activity
        .iter()
        .filter_map(|(path, e)| {
            e.last_active_at
                .map(|t| (path.clone(), (t / 2.0).floor() * 2.0))
        })
        .collect();

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
        if let Ok(mut st) = serde_json::from_str::<thegn_core::forge::model::PrStatus>(&json) {
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

    // Pipeline roster, one table read on this (off-loop) thread — the same
    // shape as the merge-queue read above. Two derivations, both pure:
    // the sidebar's stage tag, and the `waiting_human` rows that feed the
    // EXISTING blocked evidence (no new tier, no new notification kind).
    //
    // Deliberately NOT the board's feed: the board samples only while its tab
    // is live, and the sidebar tag must stay honest with the tab closed.
    let roster = db.list_dispatches().unwrap_or_default();
    // Tell a shut board its roster moved — the only way a dispatch written by
    // another process reaches a tab that is hidden until a row exists. One hash
    // over rows already in memory; no extra I/O, no wake source.
    crate::monitor_pipeline::note_roster(&roster);
    status.pipeline_stages = crate::monitor_pipeline::stage_badges(&roster);
    // Third derivation off the same rows: the sidebar's compact Pipeline row.
    status.pipeline = crate::monitor_pipeline::summary(&roster);
    let stage_blocked = crate::monitor_pipeline::stage_blocked(&roster);

    // Live raised hands (OSC 9 / OSC 777), one small table read like the two
    // above. These used to arrive as unread `agent_attention` notification rows;
    // they are state now, so they clear when the user answers (THE-68).
    //
    // Folded with "keep the smaller `since`" rather than by relying on the
    // query's order: two sessions raising a hand in ONE worktree must report the
    // longest wait, which is what the tier's longest-waiting-first tie-break
    // then sorts on.
    let mut raised: BTreeMap<String, i64> = BTreeMap::new();
    for a in db.list_session_attention().unwrap_or_default() {
        raised
            .entry(a.worktree_path)
            .and_modify(|since| *since = (*since).min(a.since))
            .or_insert(a.since);
    }

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
        let (mut ci_episode, mut ci_since) = (0u64, None);
        if let Ok(Some((json, _))) = db.get_ci_cache(path)
            && let Ok(runs) = serde_json::from_str::<Vec<thegn_core::ci::CiRun>>(&json)
            && let Some(latest) = runs.first()
        {
            ci_failing = latest.state.is_failure();
            ci_running = matches!(
                latest.state,
                thegn_core::ci::CiState::Running | thegn_core::ci::CiState::Pending
            );
            // Identity + honest start time for the run, so an acknowledgement is
            // released by the *next* run rather than by cache churn or a restart.
            // Prefer the provider's run id; fall back to the commit sha.
            let key = if latest.id.is_empty() {
                &latest.sha
            } else {
                &latest.id
            };
            ci_episode = attention::episode_of(key);
            ci_since = latest
                .started_at
                .as_deref()
                .and_then(thegn_core::ci::epoch_secs);
        }
        // A real agent is bound iff `status.agent` has a non-shell entry: the
        // map is already tool-filtered in `hydrate` (yazi/lazygit/… skipped via
        // `tool_command`), so only the `"shell"`/`"local"` default sentinels
        // remain to exclude here — via the shared predicate, since this list was
        // copy-pasted in three places and drifted between them.
        let has_agent = status
            .agent
            .get(path)
            .is_some_and(|a| thegn_core::activity::is_real_agent(a));
        let inputs = AttentionInputs {
            activity: activity_kind,
            activity_since,
            unread: unread.remove(path).unwrap_or_default(),
            pr: pr.get(path).copied(),
            ci_failing,
            ci_running,
            ci_episode,
            ci_since,
            merge_queue: mq.get(path).copied(),
            dirty: status.git.get(path).is_some_and(|g| g.dirty),
            has_agent,
            stage_blocked_since: stage_blocked.get(path).copied(),
            attention_signal_since: raised.get(path).copied(),
        };
        scores.insert(path.clone(), attention::score(&inputs));
    }

    // Acknowledgements: an acked worktree is suppressed from the nag surfaces
    // (badge + "Needs you" popup) while its current score still matches the
    // acked `(reason, since, episode)`.
    //
    // This pass is READ-ONLY apart from unparseable rows. A non-match means only
    // "that signal isn't the winner *right now*" — not that the ack is stale —
    // and it happens routinely for two benign reasons:
    //
    //   * a transient cache dip. `ci_refresh` writes the CI cache on any `Ok`,
    //     including an empty run list, which blanks `ci_failing` for a pass.
    //   * being outranked. `score` reports only the most urgent signal, so an
    //     arriving `agent_attention` hides an acked CI failure until it's read.
    //
    // The old code deleted on any mismatch, and this runs on *every* hydration
    // pass (every refresh tick) — so one such pass destroyed the ack forever and
    // the signal re-nagged as soon as it resurfaced, most visibly across a
    // restart. Staleness is instead handled where it is actually knowable: the
    // UPSERT when the user acks a new episode, the `del_worktree` cascade,
    // `ack_expired` for identity-less acks, and the age sweep in `Db::open`.
    status.acked.clear();
    let now = thegn_core::util::now();
    for row in db.list_attention_acks().unwrap_or_default() {
        let Ok(reason) =
            serde_json::from_str::<thegn_core::attention::AttentionReason>(&row.reason)
        else {
            // best-effort: an unparseable reason can never match, so prune it.
            let _ = db.delete_attention_ack(&row.worktree_path, Some(&row.reason));
            continue;
        };
        let ack = thegn_core::attention::AttentionAck {
            reason,
            since: row.since,
            episode: row.episode,
        };
        if attention::ack_expired(&ack, row.acked_at, now) {
            continue;
        }
        if scores
            .get(&row.worktree_path)
            .is_some_and(|s| s.is_acked_by(&ack))
        {
            status.acked.insert(row.worktree_path);
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
    fn ack_suppresses_matching_score_and_a_new_episode_refires() {
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

        // Ack the exact showing (reason, since, episode) → suppressed next pass.
        let reason = serde_json::to_string(&sc.reason).unwrap();
        db.put_attention_ack("/wt/f", &reason, sc.since, sc.episode)
            .unwrap();
        let mut status2 = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut status2);
        assert!(status2.acked.contains("/wt/f"), "matching ack suppresses");

        // A new episode (advanced `since`) no longer matches, so the signal
        // re-fires. The row itself SURVIVES: this pass is read-only, because a
        // non-match can also just mean "not the winner right now" and deleting on
        // it destroyed acks that were still good.
        db.put_attention_ack("/wt/f", &reason, Some(sc.since.unwrap_or(0) + 1), 0)
            .unwrap();
        let mut status3 = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut status3);
        assert!(
            !status3.acked.contains("/wt/f"),
            "a new episode does not suppress"
        );
        assert_eq!(
            db.list_attention_acks().unwrap().len(),
            1,
            "the ack row is not garbage-collected by a read pass"
        );
    }

    /// The direct repro of "it came back after I cleared it": a CI failure is
    /// derived from the run cache, and `ci_refresh` writes that cache on any
    /// `Ok` — *including an empty run list*. That blanks the signal for a pass.
    /// The old code deleted the ack on any non-match, so one such pass destroyed
    /// it and the failure re-nagged the moment it reappeared.
    #[test]
    fn ack_survives_a_transient_score_dip() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree("app/c", "/repo", "/wt/c", "c", None, None)
            .unwrap();
        let failing = serde_json::to_string(&vec![thegn_core::ci::CiRun {
            id: "run-1".into(),
            state: thegn_core::ci::CiState::Fail,
            started_at: Some("2026-06-25T10:00:00Z".into()),
            ..Default::default()
        }])
        .unwrap();
        db.put_ci_cache("/wt/c", "c", &failing).unwrap();

        let session = session_with(&[("app/c", "/wt/c")]);
        let mut status = crate::sidebar::SidebarStatus::default();
        order_memo().lock().unwrap().clear();
        collect_attention(&session, &db, &mut status);
        let sc = status.attention["/wt/c"];
        assert_eq!(sc.reason, thegn_core::attention::AttentionReason::CiFailed);
        assert_ne!(sc.episode, 0, "the run id gives the failure an identity");

        // The user quiets it.
        let reason = serde_json::to_string(&sc.reason).unwrap();
        db.put_attention_ack("/wt/c", &reason, sc.since, sc.episode)
            .unwrap();
        let mut acked = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut acked);
        assert!(acked.acked.contains("/wt/c"));

        // A refresh returns an empty run list: the signal vanishes for a pass.
        db.put_ci_cache("/wt/c", "c", "[]").unwrap();
        let mut dipped = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut dipped);
        assert_eq!(
            db.list_attention_acks().unwrap().len(),
            1,
            "a transient dip must not destroy the ack"
        );
        assert!(!dipped.acked.contains("/wt/c"), "nothing to suppress");

        // The same failing run comes back → still quiet.
        db.put_ci_cache("/wt/c", "c", &failing).unwrap();
        let mut back = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut back);
        assert!(
            back.acked.contains("/wt/c"),
            "the same run must stay acknowledged across the dip"
        );

        // A genuinely NEW run is a new episode and does re-nag.
        let next_run = serde_json::to_string(&vec![thegn_core::ci::CiRun {
            id: "run-2".into(),
            state: thegn_core::ci::CiState::Fail,
            started_at: Some("2026-06-25T11:00:00Z".into()),
            ..Default::default()
        }])
        .unwrap();
        db.put_ci_cache("/wt/c", "c", &next_run).unwrap();
        let mut fresh = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut fresh);
        assert!(!fresh.acked.contains("/wt/c"), "a new run re-nags");
    }

    /// `score` reports only the most urgent signal, so an arriving
    /// `agent_attention` hides an acked CI failure until it is read. That is not
    /// staleness — and with the composite `(worktree, reason)` key the two acks
    /// coexist instead of overwriting each other.
    #[test]
    fn ack_survives_being_outranked_by_a_higher_tier_signal() {
        use thegn_core::attention::AttentionReason as R;
        let db = thegn_core::db::Db::open_memory().unwrap();
        db.put_worktree("app/o", "/repo", "/wt/o", "o", None, None)
            .unwrap();
        let failing = serde_json::to_string(&vec![thegn_core::ci::CiRun {
            id: "run-1".into(),
            state: thegn_core::ci::CiState::Fail,
            ..Default::default()
        }])
        .unwrap();
        db.put_ci_cache("/wt/o", "o", &failing).unwrap();
        let session = session_with(&[("app/o", "/wt/o")]);
        let mut status = crate::sidebar::SidebarStatus::default();
        order_memo().lock().unwrap().clear();
        collect_attention(&session, &db, &mut status);
        let ci = status.attention["/wt/o"];
        assert_eq!(ci.reason, R::CiFailed);
        db.put_attention_ack(
            "/wt/o",
            &serde_json::to_string(&ci.reason).unwrap(),
            ci.since,
            ci.episode,
        )
        .unwrap();

        // A Blocked-tier notification arrives and outranks the failure.
        let nid = db
            .put_notification("agent_attention", "x", "needs you", "/wt/o")
            .unwrap();
        let mut outranked = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut outranked);
        assert_eq!(outranked.attention["/wt/o"].reason, R::AgentNeedsInput);
        assert!(!outranked.acked.contains("/wt/o"), "the new signal nags");
        assert_eq!(
            db.list_attention_acks().unwrap().len(),
            1,
            "the CI ack must survive being outranked"
        );

        // Ack that one too — both coexist under the composite key.
        let blocked = outranked.attention["/wt/o"];
        db.put_attention_ack(
            "/wt/o",
            &serde_json::to_string(&blocked.reason).unwrap(),
            blocked.since,
            blocked.episode,
        )
        .unwrap();
        assert_eq!(db.list_attention_acks().unwrap().len(), 2);

        // Reading the notification uncovers the CI failure again — still quiet.
        db.mark_notification_read(nid).unwrap();
        let mut uncovered = crate::sidebar::SidebarStatus::default();
        collect_attention(&session, &db, &mut uncovered);
        assert_eq!(uncovered.attention["/wt/o"].reason, R::CiFailed);
        assert!(
            uncovered.acked.contains("/wt/o"),
            "the CI ack still applies once the blocker clears"
        );
    }

    #[test]
    fn repo_scope_is_the_active_worktrees_repo_and_none_when_unresolved() {
        let db = thegn_core::db::Db::open_memory().unwrap();
        for (tab, repo, path) in [
            ("app/a", "/repo/app", "/wt/a"),
            ("app/b", "/repo/app", "/wt/b"),
            ("other/c", "/repo/other", "/wt/c"),
        ] {
            let branch = tab.split('/').nth(1).unwrap();
            db.put_worktree(tab, repo, path, branch, None, None)
                .unwrap();
        }
        let session = session_with(&[("app/a", "/wt/a")]);
        let mut status = crate::sidebar::SidebarStatus::default();
        order_memo().lock().unwrap().clear();
        collect_attention(&session, &db, &mut status);
        let scope = status.repo_scope.expect("active repo resolves");
        assert!(scope.contains("/wt/a") && scope.contains("/wt/b"));
        assert!(!scope.contains("/wt/c"), "a sibling repo is out of scope");

        // Every worktree is still SCORED — scoping is a property of the nag, not
        // of the score, so the sidebar keeps rendering other repos' rows.
        assert!(status.attention.contains_key("/wt/c"));

        // Unresolvable active repo → fail open (scope nothing) rather than risk
        // hiding a signal that needs the user.
        let orphan = session_with(&[("ghost/z", "/wt/ghost")]);
        let mut s2 = crate::sidebar::SidebarStatus::default();
        collect_attention(&orphan, &db, &mut s2);
        assert!(s2.repo_scope.is_none());

        // A terminal tab (path-less group) must NOT fail open: it scopes to the
        // session's repo, so clicking a terminal doesn't flip the ✋ chip from
        // `2 +3` to `5` and back.
        let mut with_term = session_with(&[("app/a", "/wt/a")]);
        with_term.worktrees.push(WorktreeGroup::terminal("prod"));
        with_term.active = 1;
        let mut s3 = crate::sidebar::SidebarStatus::default();
        collect_attention(&with_term, &db, &mut s3);
        let scope = s3
            .repo_scope
            .expect("terminal scopes to the session's repo");
        assert!(scope.contains("/wt/a") && !scope.contains("/wt/c"));
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
