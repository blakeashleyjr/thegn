//! The help ratchet: every user-facing action must be documented by a help
//! page, or sit on the pinned allowlist (`test/help-ratchet.txt`) — which
//! may only shrink. Same philosophy as the keep-god-files-flat guidance: the debt is
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

fn context_ratchet_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test/help-context-ratchet.txt")
}

fn read_allowlist(path: PathBuf) -> Vec<String> {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_string)
        .collect()
}

fn allowlist() -> Vec<String> {
    read_allowlist(ratchet_path())
}

/// Every action id a user can bind: the host's palette specs **plus** the core
/// registry's builtins. The ratchet used to iterate only `ACTION_SPECS`, which
/// left the core-only ids (`pr-open`, `pr-create`, …)
/// bindable but undocumented with no test noticing.
fn bindable_ids() -> Vec<&'static str> {
    let mut ids: Vec<&'static str> = ACTION_SPECS.iter().map(|s| s.id).collect();
    for a in thegn_core::keymap::BUILTINS {
        if !ids.contains(&a.id) {
            ids.push(a.id);
        }
    }
    ids.sort_unstable();
    ids
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
    let _ = registry(); // best-effort: test: asserts the registry validates without panicking
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

/// The panel-section half of the same contract. A `panel:*` context nobody
/// claims silently falls back to the generic index page, so pressing F1 in
/// (say) the Logs section teaches you nothing about the Logs section.
///
/// Frozen like the action ratchet: `test/help-context-ratchet.txt` pins the
/// existing debt and may only shrink; a NEW panel section must ship a help
/// page claiming it.
#[test]
fn every_panel_context_has_a_documentation_page() {
    let reg = registry();
    let claimed: BTreeSet<&str> = reg.contexts().map(|(k, _)| k).collect();
    let allow = read_allowlist(context_ratchet_path());

    let mut sorted = allow.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        allow, sorted,
        "test/help-context-ratchet.txt must be sorted and duplicate-free"
    );

    let vocab = crate::help::context::vocabulary();
    for id in &allow {
        assert!(
            vocab.contains(id),
            "`{id}` in test/help-context-ratchet.txt is not a context key — remove the stale line"
        );
    }

    let allow: BTreeSet<String> = allow.into_iter().collect();
    let mut undocumented_new: Vec<String> = Vec::new();
    let mut now_documented: Vec<String> = Vec::new();
    for key in vocab.iter().filter(|k| k.starts_with("panel:")) {
        let documented = claimed.contains(key.as_str());
        let allowed = allow.contains(key);
        if !documented && !allowed {
            undocumented_new.push(key.clone());
        }
        if documented && allowed {
            now_documented.push(key.clone());
        }
    }
    assert!(
        undocumented_new.is_empty(),
        "panel section(s) with no help page: {undocumented_new:?}\n\
         Add `contexts: [<key>]` to a docs/help/ page.\n\
         Do NOT add to test/help-context-ratchet.txt — the allowlist only shrinks."
    );
    assert!(
        now_documented.is_empty(),
        "context(s) now claimed but still allowlisted: {now_documented:?}\n\
         Delete those lines from test/help-context-ratchet.txt to lock in the win."
    );
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
    let bindable: BTreeSet<&str> = bindable_ids().into_iter().collect();
    for id in &allow {
        assert!(
            bindable.contains(id.as_str()),
            "`{id}` in test/help-ratchet.txt is not a bindable action id \
             (ACTION_SPECS or core BUILTINS) — remove the stale line"
        );
    }

    let allow: BTreeSet<String> = allow.into_iter().collect();
    let mut undocumented_new: Vec<&str> = Vec::new();
    let mut now_documented: Vec<&str> = Vec::new();
    for id in bindable_ids() {
        let documented = documented.contains(id);
        let allowed = allow.contains(id);
        if !documented && !allowed {
            undocumented_new.push(id);
        }
        if documented && allowed {
            now_documented.push(id);
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

// ── The prose ratchet ────────────────────────────────────────────────────────
//
// `action_docs_ratchet` above checks that an action id is *claimed* in some
// page's `actions:` frontmatter. That is a cheap thing to satisfy, and it is
// exactly how the corpus drifted: coverage read ~100% while eight of twenty
// pages went untouched for a month and eight shipped features got no help
// commit at all. Claiming an id must not substitute for writing about it.
//
// So: a claimed action must also be *mentioned in the body* — by its chord or
// by a distinctive word from its label. Deliberately loose (one word is
// enough); it is a floor against zero-mention claims, not a quality bar.

fn prose_ratchet_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test/help-prose-ratchet.txt")
}

/// Words too generic to prove a page discusses a specific action.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "from", "with", "this", "that", "into", "your", "new", "open", "show",
    "toggle", "cycle", "next", "prev", "previous", "select", "built", "docs",
];

/// The human label for a bindable id, from whichever table declares it.
fn action_label(id: &str) -> Option<&'static str> {
    ACTION_SPECS
        .iter()
        .find(|s| s.id == id)
        .map(|s| s.label)
        .or_else(|| {
            thegn_core::keymap::BUILTINS
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.menu_label)
        })
}

/// Distinctive lowercase words from a label: alphanumeric runs of 4+ chars
/// that aren't stopwords.
fn label_words(label: &str) -> Vec<String> {
    label
        .split(|c: char| !c.is_alphanumeric())
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| w.len() >= 4 && !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Does `body` actually discuss the action? Its id, its chord, or any
/// distinctive label word counts.
fn body_mentions(body: &str, id: &str) -> bool {
    let hay = body.to_ascii_lowercase();
    if hay.contains(&id.to_ascii_lowercase()) {
        return true;
    }
    if let Some(chord) = crate::keymap::chord_hint_for(&thegn_core::config::Config::default(), id)
        && hay.contains(&chord.to_ascii_lowercase())
    {
        return true;
    }
    action_label(id)
        .map(|l| label_words(l).iter().any(|w| hay.contains(w.as_str())))
        .unwrap_or(false)
}

#[test]
fn claimed_actions_are_mentioned_in_the_page_body() {
    let reg = registry();
    let allow: BTreeSet<String> = read_allowlist(prose_ratchet_path()).into_iter().collect();

    let mut silent: Vec<String> = Vec::new();
    let mut now_written: Vec<String> = Vec::new();
    for page in reg.pages().iter().filter(|p| !p.meta.generated) {
        for id in &page.meta.actions {
            let key = format!("{}:{id}", page.meta.id);
            let mentioned = body_mentions(&page.body, id);
            if !mentioned && !allow.contains(&key) {
                silent.push(key);
            } else if mentioned && allow.contains(&key) {
                now_written.push(key);
            }
        }
    }
    silent.sort();
    now_written.sort();

    assert!(
        silent.is_empty(),
        "action(s) claimed in frontmatter but never mentioned in the body:\n  {}\n\
         Write a sentence about them — naming the chord or the action — rather than \
         only listing the id.\n\
         Do NOT add to test/help-prose-ratchet.txt; the allowlist only shrinks.",
        silent.join("\n  ")
    );
    assert!(
        now_written.is_empty(),
        "now documented in prose but still allowlisted:\n  {}\n\
         Delete those lines from test/help-prose-ratchet.txt to lock in the win.",
        now_written.join("\n  ")
    );
}

/// Regenerate the prose allowlist. Same gate as `help_ratchet_update`.
#[test]
#[ignore = "writes test/help-prose-ratchet.txt; run via `just help-ratchet-update`"]
fn help_prose_ratchet_update() {
    if std::env::var("THEGN_HELP_RATCHET_UPDATE").as_deref() != Ok("1") {
        return;
    }
    let reg = registry();
    let mut lines = vec![
        "# help-prose-ratchet — `<page>:<action>` pairs whose page claims the".to_string(),
        "# action in `actions:` frontmatter but never mentions it in the body.".to_string(),
        "# Claiming an id must not substitute for writing about it.".to_string(),
        "# This list may only SHRINK (or run `just help-ratchet-update`).".to_string(),
    ];
    let mut silent: BTreeSet<String> = BTreeSet::new();
    for page in reg.pages().iter().filter(|p| !p.meta.generated) {
        for id in &page.meta.actions {
            if !body_mentions(&page.body, id) {
                silent.insert(format!("{}:{id}", page.meta.id));
            }
        }
    }
    lines.extend(silent);
    std::fs::write(prose_ratchet_path(), lines.join("\n") + "\n")
        .expect("write help-prose-ratchet.txt");
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
        "# help-ratchet — bindable action ids (ACTION_SPECS + core BUILTINS) not yet".to_string(),
        "# documented by any docs/help/ page.".to_string(),
        "# This list may only SHRINK: document an action, delete its line".to_string(),
        "# (or run `just help-ratchet-update`). New actions must be documented".to_string(),
        "# immediately — the ratchet test refuses additions.".to_string(),
    ];
    lines.extend(
        bindable_ids()
            .into_iter()
            .map(str::to_string)
            .filter(|id| !documented.contains(id))
            .collect::<BTreeSet<_>>(),
    );
    std::fs::write(ratchet_path(), lines.join("\n") + "\n").expect("write help-ratchet.txt");
}
