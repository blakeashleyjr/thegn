//! Self-documenting legend for the sidebar's always-on worktree-row markers.
//!
//! The worktree row carries two terse markers that are otherwise opaque: a
//! teal **agent glyph** after the name (`C`, `Y`, `⊞`, …) and an amber **dirty
//! dot** on the right. On the focused (expanded) row, [`push_row_markers`]
//! spells them out on the detail line — the agent's name beside its own glyph,
//! and "uncommitted" beside the dot — so a first-time reader can decode the
//! chrome without a manual. Kept out of the pinned `chrome.rs` god-file.

use crate::seg::{Seg, Tok, seg};
use superzej_core::theme::Hue;

/// Append the focused row's marker legend to its detail segments: the running
/// `agent` (spelled out beside its own glyph) and, when `dirty`, an
/// "uncommitted" note beside the amber dot. Emits nothing for a clean,
/// agent-less row so it grows no legend.
pub fn push_row_markers(agent: Option<&str>, dirty: bool, segs: &mut Vec<Seg>) {
    if let Some(agent) = agent {
        let glyph = superzej_core::theme::agent_glyph(agent, crate::caps::agent_glyph_style());
        segs.push(seg(Tok::Hue(Hue::Teal), format!("{glyph} {agent} ")));
    }
    if dirty {
        let dot = crate::caps::active_glyphs().dot_filled;
        segs.push(seg(Tok::Hue(Hue::Amber), format!("{dot} uncommitted ")));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(segs: &[Seg]) -> String {
        segs.iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn clean_agentless_row_gets_no_legend() {
        let mut segs = Vec::new();
        push_row_markers(None, false, &mut segs);
        assert!(segs.is_empty());
    }

    #[test]
    fn agent_is_spelled_out_next_to_its_glyph() {
        let mut segs = Vec::new();
        push_row_markers(Some("yazi"), false, &mut segs);
        // Default (letter) style: the name is present so the glyph is decodable,
        // and nothing tofu-prone leaks into the letter default.
        let text = text_of(&segs);
        assert!(text.contains("yazi"), "agent name spelled out: {text:?}");
        assert!(
            text.is_ascii(),
            "letter-default legend stays ASCII: {text:?}"
        );
    }

    #[test]
    fn dirty_row_labels_the_dot() {
        let mut segs = Vec::new();
        push_row_markers(None, true, &mut segs);
        assert!(text_of(&segs).contains("uncommitted"));
    }

    #[test]
    fn both_markers_render_in_order() {
        let mut segs = Vec::new();
        push_row_markers(Some("claude"), true, &mut segs);
        let text = text_of(&segs);
        let a = text.find("claude").expect("agent first");
        let u = text.find("uncommitted").expect("dirty second");
        assert!(a < u, "agent precedes dirty note: {text:?}");
    }
}
