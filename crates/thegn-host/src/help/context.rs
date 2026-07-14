//! Context-sensitive help: map "what has focus right now" to a context key
//! (`zone:sidebar`, `panel:merge`, …) that a help page claims in its
//! `contexts:` frontmatter. Pure and total — every focus state resolves to
//! some key, and the registry falls back to `index` for unclaimed ones.

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
    out
}

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
        // No duplicates (duplicate context claims must stay detectable).
        let mut sorted = vocab.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), vocab.len());
    }
}
