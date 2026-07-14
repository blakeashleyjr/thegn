//! The help ratchet: every user-facing action must be documented by a help
//! page, or sit on the pinned allowlist (`test/help-ratchet.txt`) — which
//! may only shrink. Same philosophy as the file-size ratchet: the debt is
//! frozen, new debt is impossible.
//!
//! Regenerate the allowlist after documenting actions with
//! `just help-ratchet-update`.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::keymap_specs::ACTION_SPECS;

fn ratchet_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test/help-ratchet.txt")
}

fn allowlist() -> Vec<String> {
    let raw = std::fs::read_to_string(ratchet_path()).unwrap_or_default();
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn registry() -> thegn_core::help::HelpRegistry {
    let (reg, errors) = crate::help::pages::build_registry(&thegn_core::config::Config::default());
    assert!(
        errors.is_empty(),
        "help pages must validate cleanly:\n{}",
        errors
            .iter()
            .map(|e| format!("  - {e}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    reg
}

/// Action ids documented by *authored* pages. Generated pages don't count —
/// the keybindings page mentioning an action is tautological coverage.
fn documented_ids(reg: &thegn_core::help::HelpRegistry) -> BTreeSet<String> {
    reg.pages()
        .iter()
        .filter(|p| !p.meta.generated)
        .flat_map(|p| p.meta.actions.iter().cloned())
        .collect()
}

#[test]
fn registry_validates_cleanly() {
    let _ = registry();
}

#[test]
fn page_action_claims_are_real_action_ids() {
    let reg = registry();
    let known: BTreeSet<&str> = ACTION_SPECS
        .iter()
        .map(|s| s.id)
        .chain(thegn_core::keymap::BUILTINS.iter().map(|a| a.id))
        .collect();
    for page in reg.pages() {
        for action in &page.meta.actions {
            assert!(
                known.contains(action.as_str()),
                "page `{}` documents unknown action id `{action}` — \
                 ids must match keymap_specs::ACTION_SPECS (or core BUILTINS)",
                page.meta.id
            );
        }
    }
}

#[test]
fn every_zone_has_a_documentation_page() {
    let reg = registry();
    let claimed: BTreeSet<&str> = reg.contexts().map(|(k, _)| k).collect();
    for key in crate::help::context::vocabulary() {
        if key.starts_with("zone:") {
            assert!(
                claimed.contains(key.as_str()),
                "focus zone `{key}` has no help page claiming it — \
                 add `contexts: [{key}]` to a page in docs/help/"
            );
        }
    }
}

#[test]
fn action_docs_ratchet() {
    let reg = registry();
    let documented = documented_ids(&reg);
    let allow = allowlist();

    // The allowlist itself stays canonical: sorted, unique, no stale ids.
    let mut sorted = allow.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        allow, sorted,
        "test/help-ratchet.txt must be sorted and duplicate-free"
    );
    let spec_ids: BTreeSet<&str> = ACTION_SPECS.iter().map(|s| s.id).collect();
    for id in &allow {
        assert!(
            spec_ids.contains(id.as_str()),
            "`{id}` in test/help-ratchet.txt is not an ACTION_SPECS id — remove the stale line"
        );
    }

    let allow: BTreeSet<String> = allow.into_iter().collect();
    let mut undocumented_new: Vec<&str> = Vec::new();
    let mut now_documented: Vec<&str> = Vec::new();
    for spec in ACTION_SPECS {
        let documented = documented.contains(spec.id);
        let allowed = allow.contains(spec.id);
        if !documented && !allowed {
            undocumented_new.push(spec.id);
        }
        if documented && allowed {
            now_documented.push(spec.id);
        }
    }
    assert!(
        undocumented_new.is_empty(),
        "new action(s) without help coverage: {undocumented_new:?}\n\
         Document them: add the id to a docs/help/ page's `actions:` frontmatter.\n\
         Do NOT add to test/help-ratchet.txt — the allowlist only shrinks."
    );
    assert!(
        now_documented.is_empty(),
        "action(s) now documented but still allowlisted: {now_documented:?}\n\
         Delete those lines from test/help-ratchet.txt (or run `just help-ratchet-update`) \
         to lock in the win."
    );
}

/// The one sanctioned write: regenerate the allowlist from the current
/// undocumented set. `just help-ratchet-update` wires this up.
#[test]
#[ignore = "writes test/help-ratchet.txt; run via `just help-ratchet-update`"]
fn help_ratchet_update() {
    if std::env::var("THEGN_HELP_RATCHET_UPDATE").as_deref() != Ok("1") {
        return;
    }
    let reg = registry();
    let documented = documented_ids(&reg);
    let mut lines = vec![
        "# help-ratchet — ACTION_SPECS ids not yet documented by any docs/help/ page.".to_string(),
        "# This list may only SHRINK: document an action, delete its line".to_string(),
        "# (or run `just help-ratchet-update`). New actions must be documented".to_string(),
        "# immediately — the ratchet test refuses additions.".to_string(),
    ];
    lines.extend(
        ACTION_SPECS
            .iter()
            .map(|s| s.id.to_string())
            .filter(|id| !documented.contains(id))
            .collect::<BTreeSet<_>>(),
    );
    std::fs::write(ratchet_path(), lines.join("\n") + "\n").expect("write help-ratchet.txt");
}
