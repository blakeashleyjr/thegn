//! Drift test: `config/config.toml.example` must document every `Config`
//! section and key.
//!
//! Mechanism: walk the JSON schema of [`superzej_core::config::Config`]
//! (every struct in the config tree derives `schemars::JsonSchema`, and the
//! schema — unlike a serialized default — is immune to `skip_serializing_if`
//! guards). The walk yields the full set of (section, key) requirements,
//! treating `BTreeMap`-keyed tables (`[env.<name>]`, `[bundle.<name>]`, …) as
//! wildcard segments and `Vec<struct>` fields as `[[array-of-tables]]`.
//!
//! The example file documents most keys as comments (`# key = value`, often
//! nested like `# # key = value` inside a commented block), so the scan is
//! textual: comment markers are stripped, `[section]` / `[[section]]` headers
//! (commented or not) set the current section, and `key = …` lines register a
//! key under it. Root-level (pre-section) keys may be documented anywhere.

use serde_json::Value;
use std::collections::BTreeSet;

/// Keys/sections intentionally NOT documented in the reference example.
///
/// Every addition needs a justification comment — the default policy is that
/// every config key is documented in `config/config.toml.example`. Entries are
/// dot-joined paths matched as segment-wise prefixes; `*` is a dynamic
/// (map-keyed) segment.
const ALLOWLIST: &[&str] = &[
    // `[env.<name>.sandbox]` / `[profiles.<name>.sandbox]` are all-Option
    // overlay mirrors of the base `[sandbox]` table; every key is documented
    // once at its canonical `[sandbox]` location, and the example shows the
    // overlay pattern with a representative subset (backend/image/profile).
    "env.*.sandbox",
    "profiles.*.sandbox",
    // `[profiles.<name>.notifications]` is likewise an all-Option overlay
    // mirror of `[notifications]`; the example points at the canonical table.
    "profiles.*.notifications",
    // `[[plugins]]` manifests are developer-facing (see `plugin_api.rs`); the
    // schema (id/name/version/api/capabilities/contributions) is an internal
    // contract for bundled plugins, not an end-user configuration surface.
    "plugins",
];

fn example_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/config.toml.example")
}

/// Follow `$ref` / single-`allOf` / nullable-`anyOf` indirection to the
/// underlying schema object.
fn resolve<'a>(defs: &'a serde_json::Map<String, Value>, mut schema: &'a Value) -> &'a Value {
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
fn is_table_like(defs: &serde_json::Map<String, Value>, schema: &Value) -> bool {
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

#[derive(Default)]
struct Required {
    /// Section path patterns (dot-joined; `*` = dynamic map segment).
    sections: BTreeSet<String>,
    /// (section pattern, key name).
    keys: BTreeSet<(String, String)>,
}

fn walk(
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

/// Segment-wise pattern match: `*` in the pattern matches any one segment.
fn section_matches(pattern: &str, concrete: &str) -> bool {
    let p: Vec<&str> = pattern.split('.').collect();
    let c: Vec<&str> = concrete.split('.').collect();
    p.len() == c.len() && p.iter().zip(&c).all(|(ps, cs)| *ps == "*" || ps == cs)
}

/// Is `path` (a section pattern, optionally + key) excused by the allowlist?
/// An allowlist entry excuses itself and everything nested beneath it.
fn allowlisted(section: &str, key: Option<&str>) -> bool {
    let full: Vec<&str> = section
        .split('.')
        .filter(|s| !s.is_empty())
        .chain(key)
        .collect();
    ALLOWLIST.iter().any(|entry| {
        let e: Vec<&str> = entry.split('.').collect();
        e.len() <= full.len() && e.iter().zip(&full).all(|(es, fs)| es == fs)
    })
}

#[derive(Default)]
struct Documented {
    sections: BTreeSet<String>,
    /// (concrete section path, key name).
    keys: BTreeSet<(String, String)>,
    /// Key names seen anywhere (for root-level keys documented mid-file).
    keys_anywhere: BTreeSet<String>,
}

/// Scan the example file: strip comment markers, track `[section]` headers
/// (commented or not) and register `key = …` lines under the current section.
/// Inline tables / arrays-of-tables on the same line (`k = { a = 1 }`) register
/// their inner keys under `<section>.<k>`.
fn scan_example(text: &str) -> Documented {
    let mut doc = Documented::default();
    let mut current = String::new();
    for line in text.lines() {
        // Strip leading whitespace and any number of `#` comment markers.
        let stripped = line.trim_start_matches([' ', '\t', '#']);
        // Section header — `[a.b]` or `[[a.b]]`, alone on its line (a trailing
        // `# comment` is fine). Requiring the line to end there keeps prose
        // like `# … see [env.<name>.provider] binary_cache_*` from being
        // mistaken for a header.
        if let Some(rest) = stripped.strip_prefix('[') {
            let double = rest.starts_with('[');
            let rest = rest.strip_prefix('[').unwrap_or(rest);
            if let Some(end) = rest.find(']') {
                let name = &rest[..end];
                let mut tail = &rest[end + 1..];
                if double {
                    tail = tail.strip_prefix(']').unwrap_or(tail);
                }
                let tail = tail.trim_start();
                if !name.is_empty()
                    && (tail.is_empty() || tail.starts_with('#'))
                    && name
                        .chars()
                        .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '<' | '>'))
                {
                    current = name.to_string();
                    doc.sections.insert(current.clone());
                    continue;
                }
            }
        }
        // Key line — `key = …`.
        let is_key_char = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
        let key_end = stripped.find(|c: char| !is_key_char(c)).unwrap_or(0);
        if key_end > 0 && stripped[key_end..].trim_start().starts_with('=') {
            let key = &stripped[..key_end];
            doc.keys.insert((current.clone(), key.to_string()));
            doc.keys_anywhere.insert(key.to_string());
            // Inner keys of an inline table/array on the same line register
            // under `<section>.<key>` (e.g. `prompts = [{ type = "input" }]`).
            let rest = &stripped[key_end..];
            let sub_section = if current.is_empty() {
                key.to_string()
            } else {
                format!("{current}.{key}")
            };
            let mut chars = rest.char_indices().peekable();
            while let Some((i, c)) = chars.next() {
                if !is_key_char(c) {
                    continue;
                }
                // Walk to the end of this identifier.
                let mut end = i + c.len_utf8();
                while let Some(&(j, cj)) = chars.peek() {
                    if is_key_char(cj) {
                        end = j + cj.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                if rest[end..].trim_start().starts_with('=') {
                    doc.keys
                        .insert((sub_section.clone(), rest[i..end].to_string()));
                    // An inline table documents its sub-section too
                    // (e.g. `hints = [{ key = "…", label = "…" }]`).
                    doc.sections.insert(sub_section.clone());
                }
            }
        }
    }
    doc
}

#[test]
fn example_config_documents_every_section_and_key() {
    let schema = schemars::schema_for!(superzej_core::config::Config);
    let root = serde_json::to_value(&schema).expect("schema serializes");
    let empty = serde_json::Map::new();
    let defs = root
        .get("definitions")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    let mut required = Required::default();
    walk(defs, &root, &[], &mut required);
    assert!(
        required.keys.len() > 100,
        "schema walk looks broken: only {} keys found",
        required.keys.len()
    );

    let path = example_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let doc = scan_example(&text);

    let mut missing_sections: Vec<String> = Vec::new();
    for pattern in &required.sections {
        if allowlisted(pattern, None) {
            continue;
        }
        if !doc.sections.iter().any(|s| section_matches(pattern, s)) {
            missing_sections.push(pattern.clone());
        }
    }

    let mut missing_keys: Vec<String> = Vec::new();
    for (pattern, key) in &required.keys {
        if allowlisted(pattern, Some(key)) {
            continue;
        }
        let found = if pattern.is_empty() {
            // Root-level keys may be documented anywhere in the file (some are
            // introduced in later prose sections, e.g. `keymap_preset`).
            doc.keys_anywhere.contains(key)
        } else {
            doc.keys
                .iter()
                .any(|(s, k)| k == key && section_matches(pattern, s))
        };
        if !found {
            missing_keys.push(if pattern.is_empty() {
                key.clone()
            } else {
                format!("{pattern}.{key}")
            });
        }
    }

    assert!(
        missing_sections.is_empty() && missing_keys.is_empty(),
        "config/config.toml.example is missing documentation for \
         {} section(s) and {} key(s).\n\
         Add each below (a commented `# key = default` with a one-line doc \
         comment is fine), or allowlist it in {} with a justification.\n\n\
         missing sections ('*' = any name):\n  {}\n\n\
         missing keys (section.key):\n  {}",
        missing_sections.len(),
        missing_keys.len(),
        file!(),
        missing_sections.join("\n  "),
        missing_keys.join("\n  "),
    );
}

/// The example must also stay *parseable* as the real `Config` once the
/// comment markers are removed from the live (uncommented) keys — i.e. the
/// file as shipped is valid TOML that deserializes into `Config`.
#[test]
fn example_config_parses_as_config() {
    let path = example_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let cfg: Result<superzej_core::config::Config, _> = toml::from_str(&text);
    if let Err(e) = cfg {
        panic!("config.toml.example does not parse as Config: {e}");
    }
}
