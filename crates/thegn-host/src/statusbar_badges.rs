//! Statusbar badge chips (the always-on right-cluster indicators), extracted
//! from `chrome::statusbar_items` (chrome.rs is pinned by the file-size
//! ratchet). Each `push_*` appends its chip(s) to the ordered item list; all
//! stay silent when clean (the "clean is quiet" posture).

use crate::chrome::{BarBadge, BarItemId, FrameModel};
use crate::seg::{Seg, Tok};
use thegn_core::theme::Hue;

/// Needs-you chip: how many worktrees currently need the user (attention
/// tiers T0–T2 — blocked on input, failures, finished-awaiting-review). Red
/// while anything is blocked/failing, amber when only finished work waits.
/// Activating it drills into the list; `Alt a` jumps to the next one.
pub(crate) fn push_attention_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use thegn_core::attention::AttentionTier;
    let status = &model.sidebar_status;
    let active = crate::handlers::attention::active_worktree_path(model);
    let (mut n, mut urgent) = (0usize, false);
    // Acknowledged (quieted) worktrees and the focused worktree don't count —
    // the badge tracks the same needs-you set the "Needs you" popup shows (see
    // `needs_user_ordered`).
    for (_, s) in status.attention.iter().filter(|(p, s)| {
        s.needs_user() && !status.acked.contains(p.as_str()) && Some(p.as_str()) != active
    }) {
        n += 1;
        urgent |= s.tier <= AttentionTier::Failure;
    }
    if n == 0 {
        return;
    }
    let hue = if urgent { Hue::Red } else { Hue::Amber };
    let hand = crate::caps::active_glyphs().attention;
    items.push((
        BarItemId::Badge(BarBadge::Attention),
        vec![Seg::chip(Tok::Hue(hue), format!(" {hand} {n} "))],
    ));
}

/// Persistent-pane chip: a quiet dim `◆ persist` while the focused pane is
/// daemon-backed — quitting the UI detaches it (the process keeps running;
/// the next launch warm-reattaches). ASCII terminals get `* persist`.
pub(crate) fn push_daemon_chip(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    if !model.persistent_pane {
        return;
    }
    let mark = crate::caps::active_glyphs().diamond_filled;
    items.push((
        BarItemId::Badge(BarBadge::Persist),
        vec![Seg::chip(Tok::Hue(Hue::Teal), format!(" {mark} persist "))],
    ));
}

/// CI rollup badge (AV group, item 158): a red ✗ chip when workflows are
/// *currently* failing, an amber ● chip while runs are in flight; silent when
/// all green (mirrors the "clean is quiet" notification posture). Only when CI
/// is configured and the cache is warm (`ci_runs` non-empty). Counts come from
/// `current_summary` — each workflow judged by its most recent run — so
/// historical failures don't pin the badge red.
pub(crate) fn push_ci_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    if model.panel.ci_runs.is_empty() {
        return;
    }
    let cur = thegn_core::ci::current_summary(&model.panel.ci_runs);
    let fail = cur.failed;
    let running = cur.running;
    if fail > 0 {
        items.push((
            BarItemId::Badge(BarBadge::Ci),
            vec![Seg::chip(
                Tok::Hue(Hue::Red),
                format!(" {} {fail} CI ", crate::caps::active_glyphs().cross),
            )],
        ));
    } else if running > 0 {
        items.push((
            BarItemId::Badge(BarBadge::Ci),
            vec![Seg::chip(
                Tok::Hue(Hue::Amber),
                format!(" {} {running} CI ", crate::caps::active_glyphs().dot_filled),
            )],
        ));
    }
}

/// Merge-queue (fold-actor) badge: a red ⚑ chip when branches are blocked
/// (deferred / gate-failed / needs-human), an amber chip while the queue is
/// working (folding / agent running), and a quiet dim chip whenever anything
/// is merely queued or held at ready — so an idle-but-populated queue is
/// visible. Silent only when the queue is empty (clean is quiet). Activating
/// it opens the queue overlay (`detail.rs`).
pub(crate) fn push_mq_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    let q = &model.panel.merge_queue;
    let blocked = q
        .iter()
        .filter(|r| {
            matches!(
                r.status.as_str(),
                "deferred" | "gate_failed" | "needs_human"
            )
        })
        .count();
    let working = q
        .iter()
        .filter(|r| matches!(r.status.as_str(), "folding" | "verifying" | "agent_running"))
        .count();
    let idle = q
        .iter()
        .filter(|r| matches!(r.status.as_str(), "queued" | "ready"))
        .count();
    if blocked > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(Tok::Hue(Hue::Red), format!(" ⚑ {blocked} MQ "))],
        ));
    } else if working > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(Tok::Hue(Hue::Amber), format!(" ⧉ {working} MQ "))],
        ));
    } else if idle > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(
                Tok::Slot(crate::chrome::S::Dim),
                format!(" ⧉ {idle} MQ "),
            )],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use thegn_core::attention::{AttentionReason, AttentionScore, AttentionTier};

    fn score(tier: AttentionTier) -> AttentionScore {
        AttentionScore {
            tier,
            sub: 0,
            reason: AttentionReason::AgentWaiting,
            since: None,
        }
    }

    fn chip_text(items: &[(BarItemId, Vec<Seg>)]) -> String {
        items
            .iter()
            .flat_map(|(_, segs)| segs.iter().map(|s| s.text.clone()))
            .collect()
    }

    #[test]
    fn ci_badge_reflects_current_state_not_history() {
        use thegn_core::ci::{CiRun, CiState};
        let run = |id: &str, name: &str, state| CiRun {
            id: id.into(),
            name: name.into(),
            state,
            ..Default::default()
        };
        let mut model = FrameModel::default();
        // Newest-first: the "ci" workflow passes now but failed twice before —
        // the badge must stay quiet (the old all-runs count showed "✗ 2 CI").
        model.panel.ci_runs = vec![
            run("4", "ci", CiState::Pass),
            run("3", "ci", CiState::Fail),
            run("2", "ci", CiState::Fail),
        ];
        let mut items = Vec::new();
        push_ci_badge(&model, &mut items);
        assert!(items.is_empty(), "green-now pipeline must be quiet");
        // A currently-failing workflow counts exactly once.
        model
            .panel
            .ci_runs
            .insert(0, run("9", "lint", CiState::Fail));
        push_ci_badge(&model, &mut items);
        assert!(chip_text(&items).contains(" 1 CI"));
    }

    #[test]
    fn attention_badge_counts_needs_user_and_hues_by_urgency() {
        let mut model = FrameModel::default();
        let mut items = Vec::new();
        // Nothing needing the user: silent.
        push_attention_badge(&model, &mut items);
        assert!(items.is_empty());

        // Two waiting + one working (not counted): amber chip " _ 2 ".
        let st = &mut model.sidebar_status;
        st.attention
            .insert("/a".into(), score(AttentionTier::Waiting));
        st.attention
            .insert("/b".into(), score(AttentionTier::Waiting));
        st.attention
            .insert("/c".into(), score(AttentionTier::Working));
        push_attention_badge(&model, &mut items);
        assert_eq!(items.len(), 1);
        assert!(chip_text(&items).contains(" 2 "));
        assert!(matches!(items[0].0, BarItemId::Badge(BarBadge::Attention)));

        // A blocked worktree makes it urgent (red) and counts too.
        model
            .sidebar_status
            .attention
            .insert("/d".into(), score(AttentionTier::Blocked));
        let mut items = Vec::new();
        push_attention_badge(&model, &mut items);
        assert!(chip_text(&items).contains(" 3 "));
    }

    fn mq_row(status: &str) -> thegn_core::db::MergeQueueRow {
        thegn_core::db::MergeQueueRow {
            worktree: format!("/wt/{status}"),
            branch: format!("b-{status}"),
            target_branch: "main".into(),
            status: status.into(),
            queued_at: 1,
            updated_at: 1,
            result_oid: None,
            conflict_paths: None,
            error_detail: None,
        }
    }

    fn mq_chip_for(statuses: &[&str]) -> Option<(String, Seg)> {
        let mut model = FrameModel::default();
        model.panel.merge_queue = statuses.iter().map(|s| mq_row(s)).collect();
        let mut items = Vec::new();
        push_mq_badge(&model, &mut items);
        items
            .pop()
            .map(|(_, mut segs)| (segs[0].text.clone(), segs.remove(0)))
    }

    #[test]
    fn mq_badge_hues_by_severity_and_shows_idle_queues() {
        // Empty queue: silent (clean is quiet).
        assert!(mq_chip_for(&[]).is_none());
        // Merely queued / held at ready: a quiet dim chip — the queue must be
        // discoverable even when nothing is running or failing.
        let (text, seg) = mq_chip_for(&["queued", "ready"]).unwrap();
        assert!(text.contains("2 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Slot(crate::chrome::S::Dim))); // chips carry the tone as bg
        // Working (agent included) wins over idle: amber.
        let (text, seg) = mq_chip_for(&["queued", "agent_running"]).unwrap();
        assert!(text.contains("1 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Amber))); // chips carry the tone as bg
        // Anything blocked (needs_human included) wins over all: red ⚑.
        let (text, seg) = mq_chip_for(&["queued", "folding", "needs_human"]).unwrap();
        assert!(text.contains("⚑ 1 MQ"), "{text}");
        assert_eq!(seg.bg, Some(Tok::Hue(Hue::Red))); // chips carry the tone as bg
        // Only landed rows: nothing left to signal.
        assert!(mq_chip_for(&["landed"]).is_none());
    }
}
