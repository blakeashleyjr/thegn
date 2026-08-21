//! The one merged view of "every key the app responds to".
//!
//! Bindings arrive from three unrelated places and no single table has ever
//! held all of them:
//!
//! 1. the **core registry** — `thegn_core::keymap::effective`, i.e. the legacy
//!    `BUILTINS` table after `[keybinds]` overrides and `[[actions]]`;
//! 2. the **host action specs** — `keymap_specs::ACTION_SPECS`, which is where
//!    the overwhelming majority of real actions live today;
//! 3. the **zone-local key tables** — keys a focused zone handles itself,
//!    outside the registry entirely (the sidebar, and each panel section).
//!
//! `thegn keys list` folded all three; the generated Keybindings help page
//! folded only (1), so it silently omitted ~89 of ~123 actions — including
//! `palette`, `zoom`, `quit` and both splits — while still promising to show
//! "every key". This module is that fold, extracted so the CLI and the help
//! page cannot disagree again. Pure and config-driven: no I/O, no globals.

use std::collections::BTreeSet;

use thegn_core::config::Config;
use thegn_core::keymap;

/// Where a binding was declared. The string forms are the stable values
/// `thegn keys list --json` emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A core `BUILTINS` entry (possibly rebound via `[keybinds]`).
    Registry,
    /// A user-defined `[[actions]]` entry.
    Config,
    /// A host `ACTION_SPECS` entry the core registry doesn't carry.
    Host,
    /// A zone-local table: handled by the focused zone, not rebindable today.
    ZoneTable,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Registry => "registry",
            Source::Config => "config",
            Source::Host => "host",
            Source::ZoneTable => "zone-table",
        }
    }
}

/// One binding, from whichever source declared it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    /// Display-form chords (`"Ctrl-Alt-s"`), in declaration order. **Empty
    /// means palette-only** — no default chord, but still bindable by id.
    pub chords: Vec<String>,
    pub id: String,
    pub label: String,
    /// Where the binding applies: `global`, `center`, `sidebar`, `panel`,
    /// `masthead`, `statusbar`, `bars`, or `panel:<section-key>`.
    pub zone: String,
    pub source: Source,
}

impl Binding {
    /// Whether this action has no default chord and must be run from the
    /// palette (or bound by id in `[keybinds]`).
    pub fn palette_only(&self) -> bool {
        self.chords.is_empty()
    }
}

/// Human name for a registry context. These double as the help page's section
/// headings, so the CLI's `── sidebar ──` groups and the page's `## Sidebar`
/// sections describe the same set.
pub fn context_name(c: keymap::Context) -> &'static str {
    match c {
        keymap::Context::Global => "global",
        keymap::Context::Center => "center",
        keymap::Context::Left => "sidebar",
        keymap::Context::Right => "panel",
        keymap::Context::Top => "masthead",
        keymap::Context::Bottom => "statusbar",
        keymap::Context::TopAndBottom => "bars",
    }
}

/// The order zones are presented in, coarse-to-specific. Zones not listed
/// (`panel:<section>`) sort after these, alphabetically.
const ZONE_ORDER: &[&str] = &[
    "global",
    "center",
    "sidebar",
    "panel",
    "masthead",
    "statusbar",
    "bars",
];

/// Sort key for a zone: its index in [`ZONE_ORDER`], else past the end.
fn zone_rank(zone: &str) -> usize {
    ZONE_ORDER
        .iter()
        .position(|z| *z == zone)
        .unwrap_or(ZONE_ORDER.len())
}

/// Every binding the app responds to, for `cfg`. Sorted by zone (in
/// [`ZONE_ORDER`]), then by id, so both consumers render deterministically.
pub fn collect(cfg: &Config) -> Vec<Binding> {
    let mut out = Vec::new();

    // 1. The core registry: builtins + [keybinds] overrides + [[actions]].
    for a in keymap::effective(cfg) {
        out.push(Binding {
            chords: a.chords.iter().map(|c| c.to_hint()).collect(),
            id: a.id.clone(),
            label: a.menu_label.clone(),
            zone: a
                .contexts
                .first()
                .copied()
                .map(context_name)
                .unwrap_or("global")
                .to_string(),
            source: if a.custom {
                Source::Config
            } else {
                Source::Registry
            },
        });
    }

    // 2. Host action specs the core registry doesn't carry. Chordless specs are
    //    kept (with an empty `chords`) rather than dropped: they are bindable by
    //    id and runnable from the palette, so omitting them is exactly the gap
    //    this module exists to close.
    let seen: BTreeSet<String> = out.iter().map(|b| b.id.clone()).collect();
    for spec in crate::keymap::action_specs() {
        if seen.contains(spec.id) {
            continue;
        }
        out.push(Binding {
            chords: crate::keymap::chord_hint_for(cfg, spec.id)
                .into_iter()
                .collect(),
            id: spec.id.to_string(),
            label: spec.label.to_string(),
            zone: "global".to_string(),
            source: Source::Host,
        });
    }

    // 3. Zone-local tables — keys handled by a focused zone, outside the
    //    registry entirely (and therefore not rebindable today).
    for e in crate::sidebar_keytable::SIDEBAR_KEYS {
        out.push(Binding {
            chords: vec![e.chord.to_string()],
            id: format!("sidebar:{:?}", e.id),
            label: e.label.to_string(),
            zone: "sidebar".to_string(),
            source: Source::ZoneTable,
        });
    }
    // Panel sections: the row-mode action keys each section claims. `key: None`
    // rows are the shared accordion navigation, already listed once per zone.
    for section in crate::panel::SECTION_ORDER {
        for sk in crate::panel::section_keys::section_keys(section) {
            let Some(c) = sk.key else { continue };
            out.push(Binding {
                chords: vec![sk.chord.to_string()],
                id: format!("panel:{}:{c}", section.as_key()),
                label: sk.label.to_string(),
                zone: format!("panel:{}", section.as_key()),
                source: Source::ZoneTable,
            });
        }
    }

    out.sort_by(|a, b| {
        (zone_rank(&a.zone), &a.zone, &a.id).cmp(&(zone_rank(&b.zone), &b.zone, &b.id))
    });
    out
}

/// The distinct zones present in `bindings`, in [`collect`]'s sort order.
pub fn zones(bindings: &[Binding]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for b in bindings {
        if out.last() != Some(&b.zone) {
            out.push(b.zone.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_all_three_sources() {
        let b = collect(&Config::default());
        for source in [Source::Registry, Source::Host, Source::ZoneTable] {
            assert!(
                b.iter().any(|x| x.source == source),
                "{source:?} rows present"
            );
        }
    }

    /// The regression this module exists for: host actions that live only in
    /// `ACTION_SPECS` must be present. Before the fold these were invisible to
    /// the generated help page.
    #[test]
    fn host_only_actions_are_included() {
        let b = collect(&Config::default());
        for id in [
            "palette",
            "zoom",
            "quit",
            "split-down",
            "split-right",
            "search-pane",
            "cycle-theme",
        ] {
            assert!(b.iter().any(|x| x.id == id), "`{id}` folded in");
        }
    }

    /// Chordless host specs are kept as palette-only rather than dropped.
    #[test]
    fn chordless_host_specs_survive_as_palette_only() {
        let b = collect(&Config::default());
        let row = b
            .iter()
            .find(|x| x.id == "delete-workspace")
            .expect("chordless host spec listed");
        assert!(row.palette_only(), "no default chord: {row:?}");
    }

    #[test]
    fn rebinds_are_reflected() {
        let mut cfg = Config::default();
        cfg.keybinds
            .insert("toggle-sidebar".into(), "Ctrl Alt d".into());
        let b = collect(&cfg);
        let row = b.iter().find(|x| x.id == "toggle-sidebar").unwrap();
        assert_eq!(row.chords, vec!["Ctrl-Alt-d".to_string()]);
    }

    #[test]
    fn ids_are_unique() {
        let b = collect(&Config::default());
        let mut ids: Vec<&str> = b.iter().map(|x| x.id.as_str()).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "no duplicate ids across the fold");
    }

    #[test]
    fn zones_are_ordered_coarse_to_specific() {
        let b = collect(&Config::default());
        let z = zones(&b);
        assert_eq!(z.first().map(String::as_str), Some("global"));
        // Panel-section zones sort after the coarse ones.
        let first_section = z.iter().position(|x| x.starts_with("panel:"));
        let last_coarse = z.iter().rposition(|x| ZONE_ORDER.contains(&x.as_str()));
        if let (Some(f), Some(l)) = (first_section, last_coarse) {
            assert!(f > l, "panel:* zones come last: {z:?}");
        }
        // `zones` is deduped and contiguous with the sort.
        let mut sorted = z.clone();
        sorted.dedup();
        assert_eq!(sorted, z, "zones are already grouped");
    }
}
