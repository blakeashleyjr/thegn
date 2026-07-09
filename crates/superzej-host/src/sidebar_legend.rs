//! Self-documenting legend for the sidebar's always-on worktree-row markers.
//!
//! The worktree row carries one otherwise-opaque marker: an amber **dirty dot**
//! on the right. On the focused (expanded) row, [`push_row_markers`] spells it
//! out on the detail line — "uncommitted" beside the dot — so a first-time
//! reader can decode the chrome without a manual. (The old teal agent/app glyph
//! was dropped from the row entirely, so it no longer needs a legend.) Kept out
//! of the pinned `chrome.rs` god-file.

use crate::seg::{Seg, Tok, seg};
use superzej_core::attention::{AttentionScore, AttentionTier};
use superzej_core::theme::Hue;

/// Append the focused row's marker legend to its detail segments: when `dirty`,
/// an "uncommitted" note beside the amber dot. Emits nothing for a clean row so
/// it grows no legend.
pub fn push_row_markers(dirty: bool, segs: &mut Vec<Seg>) {
    if dirty {
        let dot = crate::caps::active_glyphs().dot_filled;
        segs.push(seg(Tok::Hue(Hue::Amber), format!("{dot} uncommitted ")));
    }
}

/// Append the row's attention reason — glyph + short label ("agent needs
/// input", "CI failed", "ready to land"), hued by tier — so a row's placement
/// under the Attention sort is always explainable from its detail line. Emits
/// nothing for an idle row.
pub fn push_attention_reason(score: Option<&AttentionScore>, segs: &mut Vec<Seg>) {
    let Some(score) = score.filter(|s| s.tier != AttentionTier::Idle) else {
        return;
    };
    let g = crate::caps::active_glyphs();
    let (glyph, hue) = match score.tier {
        AttentionTier::Blocked => (g.attention, Hue::Red),
        AttentionTier::Failure => (g.cross, Hue::Red),
        AttentionTier::Waiting => (g.dot_filled, Hue::Amber),
        AttentionTier::Ready => (g.diamond_filled, Hue::Green),
        AttentionTier::Working => (g.dot_filled, Hue::Teal),
        AttentionTier::Idle => unreachable!(),
    };
    segs.push(seg(
        Tok::Hue(hue),
        format!("{glyph} {} ", score.reason.label()),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(segs: &[Seg]) -> String {
        segs.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn clean_row_gets_no_legend() {
        let mut segs = Vec::new();
        push_row_markers(false, &mut segs);
        assert!(segs.is_empty());
    }

    #[test]
    fn dirty_row_labels_the_dot() {
        let mut segs = Vec::new();
        push_row_markers(true, &mut segs);
        assert!(text_of(&segs).contains("uncommitted"));
    }

    #[test]
    fn attention_reason_spells_out_non_idle_tiers() {
        use superzej_core::attention::{AttentionReason, AttentionScore, AttentionTier};
        // Idle (or absent) emits nothing.
        let mut segs = Vec::new();
        push_attention_reason(None, &mut segs);
        push_attention_reason(Some(&AttentionScore::default()), &mut segs);
        assert!(segs.is_empty());

        // A blocked row spells out why it floated to the top.
        let blocked = AttentionScore {
            tier: AttentionTier::Blocked,
            sub: 0,
            reason: AttentionReason::AgentNeedsInput,
            since: Some(1),
        };
        push_attention_reason(Some(&blocked), &mut segs);
        assert!(text_of(&segs).contains("agent needs input"));

        // Every non-idle tier renders a non-empty legend.
        for (tier, reason) in [
            (AttentionTier::Failure, AttentionReason::CiFailed),
            (AttentionTier::Waiting, AttentionReason::AgentWaiting),
            (AttentionTier::Ready, AttentionReason::ReadyToLand),
            (AttentionTier::Working, AttentionReason::AgentWorking),
        ] {
            let mut segs = Vec::new();
            let s = AttentionScore {
                tier,
                sub: 0,
                reason,
                since: None,
            };
            push_attention_reason(Some(&s), &mut segs);
            assert!(text_of(&segs).contains(reason.label()), "{tier:?}");
        }
    }
}
