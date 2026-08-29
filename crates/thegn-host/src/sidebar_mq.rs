//! Host rendering adapter for the compact workspace merge-queue token.
//!
//! Queue policy lives in `thegn_core::merge_queue_view`; this module only
//! resolves the existing capability glyphs and host palette tokens and keeps
//! the measured width beside the segments that paint it.

use thegn_core::attention::MqStatus;
use thegn_core::merge_queue_view::{MqRollup, MqTier, MqTokenFit, fit_token};
use thegn_core::theme::Hue;

use crate::chrome::S;
use crate::seg::{Seg, Tok, seg, seg_width};

/// The painted token and its measured display width.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MqToken {
    pub segments: Vec<Seg>,
    pub width: usize,
}

/// Render the fullest queue token that fits in `available` display columns.
/// The count is dropped before the marker, and the marker is dropped last.
pub(crate) fn token(rollup: MqRollup, available: usize) -> Option<MqToken> {
    let marker = tier_marker(rollup.tier);
    let full = format!("{}{marker}", rollup.count);
    let marker_only = marker.to_string();
    let fg = tier_tone(rollup.tier);
    let full_width = seg_width(&[seg(fg, full.clone())]);
    let marker_width = seg_width(&[seg(fg, marker_only.clone())]);

    let (text, width) = match fit_token(available, full_width, marker_width) {
        MqTokenFit::Full => (full, full_width),
        MqTokenFit::MarkerOnly => (marker_only, marker_width),
        MqTokenFit::Hidden => return None,
    };

    let segments = vec![seg(fg, text)];
    debug_assert_eq!(seg_width(&segments), width);
    Some(MqToken { segments, width })
}

/// The semantic tone used by the full token and the rail's existing initial.
pub(crate) fn tier_tone(tier: MqTier) -> Tok {
    match tier {
        MqTier::Blocked => Tok::Hue(Hue::Red),
        MqTier::Working => Tok::Hue(Hue::Amber),
        // Populated is intentionally quiet; the marker still carries the
        // queue vocabulary while the palette remains dim.
        MqTier::Populated => Tok::Slot(S::Dim),
    }
}

/// Rail mode has no room for a second queue token. It may tint the existing
/// workspace initial, but populated queues remain neutral.
pub(crate) fn rail_tone(rollup: Option<MqRollup>) -> Tok {
    rollup
        .map(|r| match r.tier {
            MqTier::Blocked => Tok::Hue(Hue::Red),
            MqTier::Working => Tok::Hue(Hue::Amber),
            MqTier::Populated => Tok::Slot(S::Text),
        })
        .unwrap_or(Tok::Slot(S::Text))
}

fn tier_marker(tier: MqTier) -> &'static str {
    let gl = crate::caps::active_glyphs();
    match tier {
        MqTier::Blocked => MqStatus::GateFailed.glyph(gl).0,
        MqTier::Working => MqStatus::Folding.glyph(gl).0,
        MqTier::Populated => MqStatus::Ready.glyph(gl).0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollup(tier: MqTier, count: usize) -> MqRollup {
        MqRollup { tier, count }
    }

    #[test]
    fn token_uses_full_then_marker_then_hidden() {
        let full = token(rollup(MqTier::Working, 2), 2).unwrap();
        assert_eq!(full.width, 2);
        assert_eq!(
            full.segments[0].text,
            format!("2{}", crate::caps::active_glyphs().dot_filled)
        );

        let marker = token(rollup(MqTier::Working, 12), 1).unwrap();
        assert_eq!(marker.width, 1);
        assert_eq!(
            marker.segments[0].text,
            crate::caps::active_glyphs().dot_filled
        );

        assert!(token(rollup(MqTier::Working, 2), 0).is_none());
    }

    #[test]
    fn token_tone_matches_queue_tier() {
        assert_eq!(tier_tone(MqTier::Blocked), Tok::Hue(Hue::Red));
        assert_eq!(tier_tone(MqTier::Working), Tok::Hue(Hue::Amber));
        assert_eq!(tier_tone(MqTier::Populated), Tok::Slot(S::Dim));
    }
}
