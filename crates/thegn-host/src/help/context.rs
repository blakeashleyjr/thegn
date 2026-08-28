//! Context-sensitive help: map "what has focus right now" to a context key
//! (`zone:sidebar`, `panel:merge`, …) that a help page claims in its
//! `contexts:` frontmatter. Pure and total — every focus state resolves to
//! some key, and the registry falls back to `index` for unclaimed ones.
//!
//! The vocabulary is not only zones and panel sections. A full-screen modal
//! that owns the keyboard — the system monitor — is neither, so [`resolve`]
//! would answer for whatever is focused *behind* it; those surfaces carry an
//! `overlay:*` key of their own and open help at it directly (see
//! `MonitorOutcome::Help`). They are in [`vocabulary`] so a page may claim one,
//! but never returned by [`resolve`], which only ever sees the focus state.

use crate::focus::Zone;
use crate::panel::{PanelUi, SECTION_ORDER, Section};

/// The `zone:*` key for a focus zone. `Panel` resolves through the open
/// section instead (see [`resolve`]); its zone key exists as the fallback
/// vocabulary entry for section-less states.
pub fn zone_key(zone: Zone) -> &'static str {
    match zone {
        Zone::Sidebar => "zone:sidebar",
        Zone::Center => "zone:center",
        Zone::Panel => "zone:panel",
        Zone::Drawer => "zone:drawer",
        Zone::Corner => "zone:corner",
        Zone::Masthead => "zone:masthead",
        Zone::Statusbar => "zone:statusbar",
    }
}

/// The context key for the current focus: the open panel section while the
/// panel owns the keyboard, else the zone.
pub fn resolve(focus: &crate::focus::FocusState, panel_ui: &PanelUi) -> String {
    if focus.panel() {
        return format!("panel:{}", panel_ui.open.as_key());
    }
    zone_key(focus.zone).to_string()
}

/// Every context key a help page may claim. Handed to
/// `HelpRegistry::build` so a typo'd `contexts:` entry is a validation
/// error, and iterated by the ratchet test so every zone stays documented.
pub fn vocabulary() -> Vec<String> {
    let mut out: Vec<String> = [
        Zone::Sidebar,
        Zone::Center,
        Zone::Panel,
        Zone::Drawer,
        Zone::Corner,
        Zone::Masthead,
        Zone::Statusbar,
    ]
    .iter()
    .map(|z| zone_key(*z).to_string())
    .collect();
    // Every panel section, including the two outside SECTION_ORDER.
    for s in SECTION_ORDER
        .iter()
        .chain([Section::Debug, Section::Db].iter())
    {
        out.push(format!("panel:{}", s.as_key()));
    }
    // Full-screen modals that own the keyboard, so `resolve` can't speak for
    // them. See the module doc.
    out.push(MONITOR.to_string());
    out
}

/// The system monitor's own context key. It is a modal, not a zone, so `?`/`F1`
/// inside it opens help here explicitly rather than through [`resolve`].
pub const MONITOR: &str = "overlay:monitor";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::focus::FocusState;

    #[test]
    fn panel_focus_resolves_to_the_open_section() {
        let focus = FocusState {
            zone: Zone::Panel,
            ..Default::default()
        };
        let ui = PanelUi {
            open: Section::MergeQueue,
            ..Default::default()
        };
        assert_eq!(resolve(&focus, &ui), "panel:merge");
    }

    #[test]
    fn zones_resolve_to_zone_keys() {
        let ui = PanelUi::default();
        for (zone, key) in [
            (Zone::Sidebar, "zone:sidebar"),
            (Zone::Center, "zone:center"),
            (Zone::Drawer, "zone:drawer"),
            (Zone::Corner, "zone:corner"),
            (Zone::Masthead, "zone:masthead"),
            (Zone::Statusbar, "zone:statusbar"),
        ] {
            let focus = FocusState {
                zone,
                ..Default::default()
            };
            assert_eq!(resolve(&focus, &ui), key);
        }
    }

    #[test]
    fn vocabulary_covers_zones_and_sections() {
        let vocab = vocabulary();
        assert!(vocab.iter().any(|k| k == "zone:sidebar"));
        assert!(vocab.iter().any(|k| k == "panel:merge"));
        assert!(
            vocab.iter().any(|k| k == "panel:debug"),
            "off-order sections included"
        );
        // The monitor is a modal, not a zone — but a page must still be able to
        // claim it, so the registry validates the claim rather than rejecting it.
        assert!(vocab.iter().any(|k| k == MONITOR));
        // No duplicates (duplicate context claims must stay detectable).
        let mut sorted = vocab.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), vocab.len());
    }
}
