//! Gate: every config key has a reload class.
//!
//! Walks the JSON schema of [`thegn_core::config::Config`] (the same walker
//! `config_example` uses) and requires `reload_class::class_for` to classify
//! every `(section, key)` a user can write, so a new key cannot silently wait
//! for a restart while the reload toast claims it applied. The reverse check
//! keeps `RELOAD_CLASSES` from rotting: every row must match at least one
//! schema path.

mod common;
use common::{Required, walk};
use serde_json::Value;
use std::collections::BTreeSet;
use thegn_core::config::reload_class::{RELOAD_CLASSES, class_for, pattern_matches};

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
