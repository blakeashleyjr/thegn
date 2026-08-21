//! The generated keybindings help page: the *effective* keymap — the core
//! registry, `[keybinds]` rebinds, custom `[[actions]]`, the host action
//! specs, and the zone-local key tables — rendered as markdown at
//! registry-build time, so the page always shows the user's real chords.
//!
//! The binding set comes from [`crate::keymap_merge::collect`], the same fold
//! `thegn keys list` prints. That shared source is the point: this page used
//! to build from `thegn_core::keymap::effective` alone, which carries only the
//! legacy `BUILTINS` table, so it omitted ~89 of ~123 actions — `palette`,
//! `zoom`, `quit`, both splits, every `media-*` — while `help.md` promised it
//! could "never drift".

use crate::keymap_merge::{Binding, Source, collect, zones};

/// Section heading for a zone key from the fold.
fn zone_heading(zone: &str) -> String {
    match zone {
        "global" => "Everywhere".to_string(),
        "center" => "Terminal (center)".to_string(),
        "sidebar" => "Sidebar".to_string(),
        "panel" => "Panel".to_string(),
        "masthead" => "Masthead".to_string(),
        "statusbar" => "Status bar / drawer".to_string(),
        "bars" => "Bars".to_string(),
        // `panel:<section-key>` — the section's own row-mode keys.
        other => match other.strip_prefix("panel:") {
            Some(section) => format!("Panel · {section}"),
            None => other.to_string(),
        },
    }
}

/// Escape a label for safe embedding in the help markdown subset: backticks
/// would open code spans, `[[` would open links.
fn escape(label: &str) -> String {
    label.replace('`', "'").replace("[[", "[ [")
}

/// One bullet: the chords, then the action label.
fn bullet(b: &Binding) -> String {
    let chords: Vec<String> = b.chords.iter().map(|c| format!("`{c}`")).collect();
    format!("- {} — {}\n", chords.join(" / "), escape(&b.label))
}

/// Build the full page source (frontmatter + markdown) for `cfg`.
pub fn keybindings_page(cfg: &thegn_core::config::Config) -> String {
    let bindings = collect(cfg);

    let mut out = String::from(
        "---\nid: keybindings\ntitle: Keybindings\norder: 35\ngenerated: true\n\
         contexts: [panel:keys]\n---\n\n\
         # Keybindings\n\n\
         Your **effective** keymap: the built-in defaults, `[keybinds]` rebinds, \
         custom `[[actions]]`, and the keys each zone handles itself — exactly as \
         they resolve right now. Rebind any row by id in `[keybinds]` — see \
         [[configuration]]. This is the same set `thegn keys list` prints.\n",
    );

    // Chorded bindings, grouped by zone in the fold's own order.
    for zone in zones(&bindings) {
        let rows: Vec<&Binding> = bindings
            .iter()
            .filter(|b| b.zone == zone && !b.palette_only() && b.source != Source::Config)
            .collect();
        if rows.is_empty() {
            continue;
        }
        let zone_local = rows.iter().any(|b| b.source == Source::ZoneTable);
        out.push_str(&format!("\n## {}\n\n", zone_heading(&zone)));
        for b in rows {
            out.push_str(&bullet(b));
        }
        // Zone-local keys aren't in the registry, so they can't be rebound.
        if zone_local {
            out.push_str(
                "\nSome keys here are handled by the focused zone itself rather than \
                 the registry, and are not rebindable today.\n",
            );
        }
    }

    let palette_only: Vec<&Binding> = bindings
        .iter()
        .filter(|b| b.palette_only() && b.source != Source::Config)
        .collect();
    if !palette_only.is_empty() {
        out.push_str(
            "\n## Palette-only\n\nNo default chord — run from the [[command-palette]] \
             or bind in `[keybinds]` by id.\n\n",
        );
        for b in palette_only {
            out.push_str(&format!("- {} — `{}`\n", escape(&b.label), b.id));
        }
    }

    let custom: Vec<&Binding> = bindings
        .iter()
        .filter(|b| b.source == Source::Config)
        .collect();
    if !custom.is_empty() {
        out.push_str("\n## Your actions\n\nFrom `[[actions]]` in your config.\n\n");
        for b in custom {
            out.push_str(&bullet(b));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> String {
        keybindings_page(&thegn_core::config::Config::default())
    }

    #[test]
    fn default_config_page_has_the_essentials() {
        let src = page();
        assert!(src.contains("`Alt-w`"), "default new-worktree chord");
        assert!(src.contains("New worktree"));
        assert!(src.contains("## Everywhere"));
        assert!(src.contains("## Palette-only"));
        assert!(
            !src.contains("## Your actions"),
            "no custom actions by default"
        );
    }

    /// **The regression gate.** Every bindable action the fold knows about must
    /// appear on the page. This is what would have caught the page rendering
    /// only the 34-entry legacy `BUILTINS` table while ~89 host actions —
    /// `palette`, `zoom`, `quit`, the splits — were silently missing.
    #[test]
    fn page_lists_every_bindable_action() {
        let cfg = thegn_core::config::Config::default();
        let src = keybindings_page(&cfg);
        let bindings = collect(&cfg);
        let missing: Vec<&str> = bindings
            .iter()
            .filter(|b| !src.contains(&escape(&b.label)))
            .map(|b| b.id.as_str())
            .collect();
        assert!(
            missing.is_empty(),
            "actions missing from the generated page: {missing:?}"
        );
    }

    /// Host-only actions specifically — the ones the old builder dropped.
    #[test]
    fn host_only_actions_are_rendered() {
        let src = page();
        for id in ["palette", "zoom", "quit", "split-down", "split-right"] {
            let label = crate::keymap::action_spec(id)
                .map(|s| s.label)
                .unwrap_or_else(|| panic!("{id} is a real spec"));
            assert!(src.contains(label), "`{id}` ({label}) on the page");
        }
    }

    #[test]
    fn page_parses_cleanly_through_the_help_model() {
        let src = page();
        let (meta, body) = thegn_core::help::frontmatter::parse(&src).expect("valid frontmatter");
        assert_eq!(meta.id, "keybindings");
        assert!(meta.generated);
        let blocks = thegn_core::help::markdown::parse(body);
        // Internal links may only point at pages that exist.
        for t in thegn_core::help::markdown::links(&blocks) {
            if let thegn_core::help::LinkTarget::Page(id) = t {
                assert!(
                    ["configuration", "command-palette"].contains(&id.as_str()),
                    "unexpected link target {id}"
                );
            }
        }
    }

    #[test]
    fn rebinds_show_up() {
        let mut cfg = thegn_core::config::Config::default();
        cfg.keybinds
            .insert("new-worktree".to_string(), "Ctrl Alt u".to_string());
        let src = keybindings_page(&cfg);
        assert!(src.contains("`Ctrl-Alt-u`"), "{src}");
        assert!(!src.contains("`Alt-w` — New worktree"));
    }

    #[test]
    fn escape_neutralizes_markup() {
        assert_eq!(escape("run `rm -rf`"), "run 'rm -rf'");
        assert_eq!(escape("open [[x]]"), "open [ [x]]");
    }

    #[test]
    fn zone_headings_cover_the_fold() {
        assert_eq!(zone_heading("global"), "Everywhere");
        assert_eq!(zone_heading("panel:merge"), "Panel · merge");
        assert_eq!(zone_heading("mystery"), "mystery");
    }
}
