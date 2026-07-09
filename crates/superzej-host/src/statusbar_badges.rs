//! Statusbar badge chips (the always-on right-cluster indicators), extracted
//! from `chrome::statusbar_items` (chrome.rs is pinned by the file-size
//! ratchet). Each `push_*` appends its chip(s) to the ordered item list; all
//! stay silent when clean (the "clean is quiet" posture).

use crate::chrome::{BarBadge, BarItemId, FrameModel};
use crate::seg::{Seg, Tok};
use superzej_core::theme::Hue;

/// Needs-you chip: how many worktrees currently need the user (attention
/// tiers T0–T2 — blocked on input, failures, finished-awaiting-review). Red
/// while anything is blocked/failing, amber when only finished work waits.
/// Activating it drills into the list; `Alt a` jumps to the next one.
pub(crate) fn push_attention_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use superzej_core::attention::AttentionTier;
    let scores = model.sidebar_status.attention.values();
    let (mut n, mut urgent) = (0usize, false);
    for s in scores.filter(|s| s.needs_user()) {
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

/// CI rollup badge (AV group, item 158): a red ✗ chip when recent runs have
/// failures, an amber ● chip while runs are in flight; silent when all green
/// (mirrors the "clean is quiet" notification posture). Only when CI is
/// configured and the cache is warm (`ci_runs` non-empty).
pub(crate) fn push_ci_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    use superzej_core::ci::CiState;
    if model.panel.ci_runs.is_empty() {
        return;
    }
    let fail = model
        .panel
        .ci_runs
        .iter()
        .filter(|r| r.state == CiState::Fail)
        .count();
    let running = model
        .panel
        .ci_runs
        .iter()
        .filter(|r| r.state == CiState::Running)
        .count();
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

/// Merge-queue (fold-actor) badge: a red ⚑ chip when branches are deferred and
/// need a rebase, else an amber chip while branches are queued/folding. Silent
/// when the queue is empty (clean is quiet).
pub(crate) fn push_mq_badge(model: &FrameModel, items: &mut Vec<(BarItemId, Vec<Seg>)>) {
    let q = &model.panel.merge_queue;
    let deferred = q
        .iter()
        .filter(|r| r.status == "deferred" || r.status == "gate_failed")
        .count();
    let active = q
        .iter()
        .filter(|r| matches!(r.status.as_str(), "queued" | "folding" | "verifying"))
        .count();
    if deferred > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(Tok::Hue(Hue::Red), format!(" ⚑ {deferred} MQ "))],
        ));
    } else if active > 0 {
        items.push((
            BarItemId::Badge(BarBadge::MergeQueue),
            vec![Seg::chip(Tok::Hue(Hue::Amber), format!(" ⧉ {active} MQ "))],
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use superzej_core::attention::{AttentionReason, AttentionScore, AttentionTier};

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
}
