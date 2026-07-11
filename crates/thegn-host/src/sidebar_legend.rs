//! Self-documenting legend for the sidebar's always-on worktree-row markers.
//!
//! The worktree row carries one otherwise-opaque marker: an amber **dirty dot**
//! on the right. On the focused (expanded) row, [`push_row_markers`] spells it
//! out on the detail line — "uncommitted" beside the dot — so a first-time
//! reader can decode the chrome without a manual. (The old teal agent/app glyph
//! was dropped from the row entirely, so it no longer needs a legend.) Kept out
//! of the pinned `chrome.rs` god-file.

use crate::seg::{Seg, Tok, seg};
use thegn_core::theme::Hue;

/// Append the focused row's marker legend to its detail segments: when `dirty`,
/// an "uncommitted" note beside the amber dot. Emits nothing for a clean row so
/// it grows no legend.
pub fn push_row_markers(dirty: bool, segs: &mut Vec<Seg>) {
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
}
