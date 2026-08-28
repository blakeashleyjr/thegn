//! `nix/hm-module.nix` is a hand-maintained third copy of the config schema
//! (Rust structs → `config.toml.example` → the home-manager module). Two
//! drift checks keep it honest against the Rust source of truth:
//!
//! 1. every TOML key the module's `tomlFormat.generate` block renders exists
//!    in the `Config` schema (same walker as the example-file test);
//! 2. every `lib.types.enum [...]` value list is a subset of some
//!    `config_enum!`'s accepted spellings (canonical + aliases), so the Nix
//!    option can never offer a value the binary rejects.
//!
//! The mapping is deliberately parsed from the Nix text (no Nix evaluator in
//! the test): the block is a flat attrset of `snake_key = …;`, `section.key
//! = …;`, nested `section = { … };` and `inherit (cfg.x) a b c;` lines.

mod common;

use serde_json::Value;
use std::collections::BTreeSet;

fn module_src() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nix/hm-module.nix"),
    )
    .expect("nix/hm-module.nix")
}

/// TOML key paths rendered by the `configFile = tomlFormat.generate … { … }`
/// block, as dotted `section.key` strings (top-level keys have no dot).
fn rendered_paths(src: &str) -> BTreeSet<String> {
    let start = src.find("tomlFormat.generate").expect("generate block");
    let open = src[start..].find('{').unwrap() + start;
    // Walk to the matching close brace.
    let mut depth = 0usize;
    let mut end = open;
    for (i, c) in src[open..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = open + i;
                    break;
                }
            }
            _ => {}
        }
    }
    let block = &src[open + 1..end];
    let mut out = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    for raw in block.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let prefix = |k: &str| {
            let mut p = stack.clone();
            p.push(k.to_string());
            p.join(".")
        };
        if let Some(rest) = line.strip_prefix("inherit (") {
            // `inherit (cfg.x) a b c;` — each name is a key at this level.
            if let Some((_, names)) = rest.split_once(')') {
                for n in names.trim().trim_end_matches(';').split_whitespace() {
                    out.insert(prefix(n));
                }
            }
            continue;
        }
        if line == "};" || line == "}" {
            stack.pop();
            continue;
        }
        if let Some((lhs, rhs)) = line.split_once('=') {
            let lhs = lhs.trim();
            let rhs = rhs.trim();
            if rhs == "{" {
                stack.push(lhs.to_string());
            } else {
                // `a.b = …;` is a nested key; `map (a: { … }) cfg.x` renders
                // an array of tables whose inner `inherit` names are keys
                // under `lhs`.
                if let Some(inner) = rhs.strip_prefix("map (") {
                    if let Some(i) = inner.find("inherit (") {
                        let names = inner[i + "inherit (".len()..]
                            .split_once(')')
                            .map(|(_, n)| n)
                            .unwrap_or("");
                        for n in names
                            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .filter(|n| !n.is_empty())
                            .take_while(|n| *n != "cfg")
                        {
                            out.insert(format!("{}.{n}", prefix(lhs)));
                        }
                    }
                    out.insert(prefix(lhs));
                } else {
                    out.insert(prefix(lhs));
                }
            }
        }
    }
    assert!(out.len() > 20, "generate-block parse broke: {out:?}");
    out
}

/// `lib.types.enum ["a" "b"]` value lists, with the option name they belong to.
fn enum_lists(src: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        if let Some(rest) = l.trim().strip_prefix("type = lib.types.enum [") {
            let vals: Vec<String> = rest
                .split(']')
                .next()
                .unwrap_or("")
                .split_whitespace()
                .map(|v| v.trim_matches('"').to_string())
                .collect();
            // The option name is the nearest preceding `name = lib.mkOption {`.
            let name = lines[..i]
                .iter()
                .rev()
                .find_map(|p| {
                    p.trim()
                        .strip_suffix("= lib.mkOption {")
                        .map(|n| n.trim().to_string())
                })
                .unwrap_or_else(|| format!("enum@{}", i + 1));
            out.push((name, vals));
        }
    }
    assert!(out.len() >= 8, "enum scan broke: {out:?}");
    out
}

/// Every `config_enum!` in the schema: its accepted spellings (canonical +
/// aliases), keyed by definition name.
fn schema_enums() -> Vec<(String, BTreeSet<String>)> {
    let schema = schemars::schema_for!(thegn_core::config::Config);
    let root = serde_json::to_value(&schema).unwrap();
    let mut out = Vec::new();
    for (name, def) in root["definitions"].as_object().unwrap() {
        let Some(marker) = def.get(thegn_core::config_validate::ENUM_MARKER) else {
            continue;
        };
        let mut set: BTreeSet<String> = def["enum"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        set.extend(
            marker["aliases"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string),
        );
        out.push((name.clone(), set));
    }
    out
}

#[test]
fn every_rendered_key_exists_in_the_schema() {
    let required = common::required();
    // Section patterns + (section, key) → concrete dotted paths the schema
    // allows. A rendered `section` with no key (a whole table) is fine if the
    // schema has that section.
    let schema_paths: BTreeSet<String> = required
        .keys
        .iter()
        .map(|(s, k)| {
            if s.is_empty() {
                k.clone()
            } else {
                format!("{s}.{k}")
            }
        })
        .collect();
    let sections: BTreeSet<String> = required.sections.clone();
    // Top-level map tables of scalars (`[keybinds]`) own no documented keys,
    // so the walker never lists them; accept any root property by name.
    let root_props: BTreeSet<String> = {
        let schema = schemars::schema_for!(thegn_core::config::Config);
        let root = serde_json::to_value(&schema).unwrap();
        root["properties"]
            .as_object()
            .map(|p| p.keys().cloned().collect())
            .unwrap_or_default()
    };
    let unknown: Vec<String> = rendered_paths(&module_src())
        .into_iter()
        .filter(|p| {
            !(schema_paths.iter().any(|s| common::section_matches(s, p))
                || sections.iter().any(|s| common::section_matches(s, p))
                // Map-valued top-level keys (`keybinds`, `agents`, …) appear as
                // (section, key) pairs under a `*` / array pattern; accept the
                // table name itself when any schema path starts with it.
                || schema_paths.iter().any(|s| s.starts_with(&format!("{p}.")))
                || (!p.contains('.') && root_props.contains(p)))
        })
        .collect();
    assert!(
        unknown.is_empty(),
        "nix/hm-module.nix renders keys the Config schema does not have: {unknown:?}"
    );
}

#[test]
fn every_nix_enum_is_a_subset_of_a_config_enum() {
    let enums = schema_enums();
    let mut bad = Vec::new();
    for (opt, vals) in enum_lists(&module_src()) {
        let ok = enums
            .iter()
            .any(|(_, set)| vals.iter().all(|v| set.contains(v)));
        if !ok {
            // Name the closest enum for the message.
            let best = enums
                .iter()
                .max_by_key(|(_, set)| vals.iter().filter(|v| set.contains(*v)).count())
                .map(|(n, set)| {
                    let missing: Vec<&String> = vals.iter().filter(|v| !set.contains(*v)).collect();
                    format!("{n}: not accepted {missing:?}")
                })
                .unwrap_or_default();
            bad.push(format!("option `{opt}` {vals:?} — closest {best}"));
        }
    }
    assert!(
        bad.is_empty(),
        "nix/hm-module.nix enum options offer values the binary rejects:\n{}",
        bad.join("\n")
    );
}
