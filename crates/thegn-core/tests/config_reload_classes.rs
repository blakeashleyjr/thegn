//! Gate: every config key has a reload class.
//!
//! Walks the JSON schema of [`thegn_core::config::Config`] (the same walker
//! `config_example` uses) and requires `reload_class::class_for` to classify
//! every `(section, key)` a user can write, so a new key cannot silently wait
//! for a restart while the reload toast claims it applied. The reverse check
//! keeps `RELOAD_CLASSES` from rotting: every row must match at least one
//! schema path. A third gate requires every `Live` row to name the hook that
//! applies it, so a new live key cannot silently have no applier.

mod common;
use common::{Required, walk};
use serde_json::Value;
use std::collections::BTreeSet;
use thegn_core::config::reload_class::{
    RELOAD_CLASSES, ReloadClass, ReloadHook, class_for, pattern_matches,
};

fn schema_walk() -> (Required, BTreeSet<String>) {
    let schema = schemars::schema_for!(thegn_core::config::Config);
    let root = serde_json::to_value(&schema).expect("schema to json");
    let defs = root
        .get("definitions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut req = Required::default();
    walk(&defs, &root, &[], &mut req);
    assert!(
        req.keys.len() > 100,
        "schema walk broke: {} keys",
        req.keys.len()
    );
    // Root properties, including the map-of-scalars tables the walker yields
    // no keys for (`program_remap`): a changed one still reaches the reload
    // diff as `program_remap.<name>...`, so it needs a row too.
    let roots = root
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();
    (req, roots)
}

fn dotted(section: &str, key: &str) -> String {
    if section.is_empty() {
        key.to_string()
    } else {
        format!("{section}.{key}")
    }
}

#[test]
fn every_config_key_has_a_reload_class() {
    let (req, roots) = schema_walk();
    let mut missing: Vec<String> = req
        .keys
        .iter()
        .map(|(section, key)| dotted(section, key))
        .chain(roots.iter().cloned())
        .filter(|path| class_for(path).is_none())
        .collect();
    missing.sort();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "{} config key(s) have no reload class; add a row to RELOAD_CLASSES in \
         crates/thegn-core/src/config/reload_class.rs (Live + its hook, Restart, \
         or NextUse; when unsure, Restart):\n  {}",
        missing.len(),
        missing.join("\n  ")
    );
}

#[test]
fn no_reload_class_row_is_dead() {
    let (req, roots) = schema_walk();
    let paths: Vec<String> = req
        .keys
        .iter()
        .map(|(section, key)| dotted(section, key))
        .chain(req.sections.iter().cloned())
        .chain(roots)
        .collect();
    let dead: Vec<&str> = RELOAD_CLASSES
        .iter()
        .map(|(pattern, _, _)| *pattern)
        .filter(|pattern| !paths.iter().any(|path| pattern_matches(pattern, path)))
        .collect();
    assert!(
        dead.is_empty(),
        "RELOAD_CLASSES rows match no Config schema key (renamed or removed?); \
         drop or fix them in crates/thegn-core/src/config/reload_class.rs:\n  {}",
        dead.join("\n  ")
    );
}

/// `Live` rows allowed to name [`ReloadHook::None`], each with the reason no
/// hook gates it. Everything here is applied by an unconditional swap in the
/// reload handler (run.rs), so it cannot be skipped by the plan.
const LIVE_WITHOUT_HOOK: &[(&str, &str)] = &[
    (
        "session.scrollback_lines",
        "one unconditional atomic store (snapshot::set_scrollback_lines) on every reload",
    ),
    (
        "session.resident_pool_limit",
        "one unconditional store (workspace_pool.set_limit), applied on the next park",
    ),
    (
        "model_proxy.*",
        "the model-proxy supervisor re-reads its section on every reload (spawn_supervisor)",
    ),
];

#[test]
fn every_live_row_names_a_hook() {
    let unhooked: Vec<&str> = RELOAD_CLASSES
        .iter()
        .filter(|(_, class, hook)| *class == ReloadClass::Live && *hook == ReloadHook::None)
        .map(|(pattern, _, _)| *pattern)
        .filter(|pattern| {
            !LIVE_WITHOUT_HOOK
                .iter()
                .any(|(allowed, _)| allowed == pattern)
        })
        .collect();
    assert!(
        unhooked.is_empty(),
        "Live RELOAD_CLASSES rows with no applying hook; name the ReloadHook \
         that applies them (crates/thegn-core/src/config/reload_class.rs) or add \
         them to LIVE_WITHOUT_HOOK with the reason:\n  {}",
        unhooked.join("\n  ")
    );
    // The allowlist cannot rot either: each entry is a real Live/None row.
    for (pattern, reason) in LIVE_WITHOUT_HOOK {
        assert!(!reason.is_empty());
        assert!(
            RELOAD_CLASSES
                .iter()
                .any(|row| row == &(*pattern, ReloadClass::Live, ReloadHook::None)),
            "stale LIVE_WITHOUT_HOOK entry {pattern}"
        );
    }
}
