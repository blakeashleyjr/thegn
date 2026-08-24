//! Env-override completeness: every scalar config key at the top level or in a
//! top-level section either has a `THEGN_<SECTION>_<KEY>` knob in
//! `Config::env_overlay`, or is pinned in `test/env-overlay-ratchet.txt`.
//!
//! `env_overlay` is hand-written, one line per knob ("the single place to
//! extend when a setting becomes env-settable"); nothing used to notice a new
//! key that never got its line. The allowlist is shrink-only: adding an env
//! knob forces its entry out, and a new key with no knob must be pinned with
//! a reason (most section-scoped keys legitimately aren't env-settable — the
//! file says which).
//!
//! Only depth ≤ 1 (`key`, `section.key`) is in scope: deeper tables
//! (`[sandbox.remote]`, `[notifications.priority]`) are structured config,
//! not knobs.

mod common;

use std::collections::BTreeSet;

fn env_knobs() -> BTreeSet<String> {
    let src = include_str!("../src/config.rs");
    let start = src.find("pub fn env_overlay(").expect("env_overlay fn");
    let end = src[start..]
        .find("\n}\n")
        .map(|i| start + i)
        .unwrap_or(src.len());
    let body = &src[start..end];
    let mut out = BTreeSet::new();
    let mut rest = body;
    while let Some(i) = rest.find("\"THEGN_") {
        let tail = &rest[i + 1..];
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        out.insert(name);
        rest = &rest[i + 1..];
    }
    assert!(out.len() > 40, "env_overlay scan broke: {out:?}");
    out
}

fn allowlist() -> BTreeSet<String> {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/env-overlay-ratchet.txt"),
    )
    .unwrap_or_default()
    .lines()
    .map(str::trim)
    .filter(|l| !l.is_empty() && !l.starts_with('#'))
    .map(str::to_string)
    .collect()
}

/// `section.key` → the env name the overlay convention would use.
fn env_name(section: &str, key: &str) -> String {
    let mut s = String::from("THEGN_");
    if !section.is_empty() {
        s.push_str(&section.to_ascii_uppercase());
        s.push('_');
    }
    s.push_str(&key.to_ascii_uppercase());
    s
}

#[test]
fn every_shallow_key_has_an_env_knob_or_is_pinned() {
    let knobs = env_knobs();
    let allow = allowlist();
    let required = common::required();
    let mut in_scope: BTreeSet<String> = BTreeSet::new();
    for (section, key) in &required.keys {
        // depth ≤ 1 and no wildcard segment
        if section.contains('.') || section.contains('*') {
            continue;
        }
        in_scope.insert(if section.is_empty() {
            key.clone()
        } else {
            format!("{section}.{key}")
        });
    }
    let covered = |path: &str| {
        let (section, key) = path.split_once('.').unwrap_or(("", path));
        knobs.contains(&env_name(section, key))
    };
    if std::env::var("THEGN_RATCHET_UPDATE").as_deref() == Ok("1") {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../test/env-overlay-ratchet.txt");
        let header: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .take_while(|l| l.trim().is_empty() || l.trim_start().starts_with('#'))
            .map(str::to_string)
            .collect();
        let mut out = header;
        out.push(String::new());
        out.extend(in_scope.iter().filter(|p| !covered(p)).cloned());
        std::fs::write(&path, out.join("\n") + "\n").unwrap();
        return;
    }
    let unpinned: Vec<&String> = in_scope
        .iter()
        .filter(|p| !covered(p) && !allow.contains(*p))
        .collect();
    assert!(
        unpinned.is_empty(),
        "config keys with neither a THEGN_* knob in env_overlay nor a \
         test/env-overlay-ratchet.txt entry: {unpinned:?}\n\
         Add the env line (preferred) or pin the key with a reason."
    );
    let stale: Vec<&String> = allow
        .iter()
        .filter(|p| !in_scope.contains(*p) || covered(p))
        .collect();
    assert!(
        stale.is_empty(),
        "test/env-overlay-ratchet.txt entries that now have a knob or no longer exist — \
         the list is shrink-only, delete them: {stale:?}"
    );
}

/// The `env_overlay_covers_every_knob` unit test exercises a fixed map of
/// knobs; this pins that map to the real knob set so a new env line can't be
/// added without being exercised.
#[test]
fn every_env_knob_is_exercised_by_the_coverage_test() {
    let knobs = env_knobs();
    let tests = include_str!("../src/config_tests.rs");
    let start = tests
        .find("fn env_overlay_covers_every_knob(")
        .expect("env_overlay_covers_every_knob test");
    let body = &tests[start..start + 6000];
    let missing: Vec<&String> = knobs
        .iter()
        .filter(|k| *k != "TG_PR_TTL" && !body.contains(&format!("\"{k}\"")))
        .collect();
    assert!(
        missing.is_empty(),
        "env knobs read by env_overlay but not exercised by env_overlay_covers_every_knob: {missing:?}"
    );
}
