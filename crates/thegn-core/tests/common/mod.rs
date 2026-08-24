//! Shared helpers for the config-surface integration tests: the schema
//! walker that turns `schemars::schema_for!(Config)` into the set of
//! `(section pattern, key)` pairs a user can write, so `config.toml.example`
//! coverage, env-override completeness and the home-manager module can all be
//! checked against the same source of truth (the Rust structs).

#![allow(dead_code)]

use serde_json::Value;
use std::collections::BTreeSet;

/// Follow `$ref` / single-`allOf` / nullable-`anyOf` indirection to the
/// underlying schema object.
pub fn resolve<'a>(defs: &'a serde_json::Map<String, Value>, mut schema: &'a Value) -> &'a Value {
    loop {
        if let Some(r) = schema.get("$ref").and_then(Value::as_str) {
            let name = r.rsplit('/').next().unwrap_or_default();
            match defs.get(name) {
                Some(next) => {
                    schema = next;
                    continue;
                }
                None => return schema,
            }
        }
        if let Some(all) = schema.get("allOf").and_then(Value::as_array)
            && all.len() == 1
        {
            schema = &all[0];
            continue;
        }
        if let Some(any) = schema.get("anyOf").and_then(Value::as_array) {
            let non_null: Vec<&Value> = any
                .iter()
                .filter(|v| v.get("type").and_then(Value::as_str) != Some("null"))
                .collect();
            if non_null.len() == 1 {
                schema = non_null[0];
                continue;
            }
        }
        return schema;
    }
}

/// Does this (resolved) schema describe a TOML table / array-of-tables — i.e.
/// something documented with its own `[header]` — rather than a scalar key?
pub fn is_table_like(defs: &serde_json::Map<String, Value>, schema: &Value) -> bool {
    let s = resolve(defs, schema);
    if s.get("properties")
        .and_then(Value::as_object)
        .is_some_and(|p| !p.is_empty())
    {
        return true;
    }
    if s.get("additionalProperties").is_some_and(Value::is_object) {
        return true;
    }
    if let Some(items) = s.get("items")
        && items.is_object()
    {
        return is_table_like(defs, items);
    }
    false
}

#[derive(Default, Debug)]
pub struct Required {
    /// Section path patterns (dot-joined; `*` = dynamic map segment).
    pub sections: BTreeSet<String>,
    /// (section pattern, key name).
    pub keys: BTreeSet<(String, String)>,
}

pub fn walk(
    defs: &serde_json::Map<String, Value>,
    schema: &Value,
    path: &[String],
    out: &mut Required,
) {
    let s = resolve(defs, schema);
    if let Some(props) = s.get("properties").and_then(Value::as_object) {
        let mut has_scalar_key = false;
        for (k, v) in props {
            let r = resolve(defs, v);
            if is_table_like(defs, r) {
                let mut sub = path.to_vec();
                sub.push(k.clone());
                walk(defs, r, &sub, out);
            } else {
                out.keys.insert((path.join("."), k.clone()));
                has_scalar_key = true;
            }
        }
        // Only demand a `[header]` mention for sections that directly own
        // scalar keys; purely structural sections (e.g. `[secrets]`, whose only
        // content is the dynamic `[secrets.resolvers]` map) need no header of
        // their own.
        if has_scalar_key && !path.is_empty() {
            out.sections.insert(path.join("."));
        }
    }
    // Map-valued tables (`BTreeMap<String, T>`): dynamic keys. Recurse into
    // table-like values under a `*` segment; maps of scalars need no doc'd keys.
    if let Some(ap) = s.get("additionalProperties")
        && ap.is_object()
    {
        let r = resolve(defs, ap);
        if is_table_like(defs, r) {
            let mut sub = path.to_vec();
            sub.push("*".into());
            walk(defs, r, &sub, out);
        }
    }
    // Arrays of tables (`Vec<T>` where T is a struct): `[[path]]` keeps the
    // same section path.
    if let Some(items) = s.get("items")
        && items.is_object()
    {
        let r = resolve(defs, items);
        if r.get("properties").is_some() {
            walk(defs, r, path, out);
        }
    }
}

/// The schema walk of `Config`: every section pattern and `(section, key)`.
pub fn required() -> Required {
    let schema = schemars::schema_for!(thegn_core::config::Config);
    let root = serde_json::to_value(&schema).expect("schema to json");
    let defs = root
        .get("definitions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut out = Required::default();
    walk(&defs, &root, &[], &mut out);
    assert!(
        out.keys.len() > 100,
        "schema walk broke: {} keys",
        out.keys.len()
    );
    out
}

/// Segment-wise pattern match: `*` in the pattern matches any one segment.
pub fn section_matches(pattern: &str, concrete: &str) -> bool {
    let p: Vec<&str> = pattern.split('.').collect();
    let c: Vec<&str> = concrete.split('.').collect();
    p.len() == c.len() && p.iter().zip(&c).all(|(a, b)| *a == "*" || a == b)
}
