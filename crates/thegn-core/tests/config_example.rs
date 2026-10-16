//! Drift test: `config/config.toml.example` must document every `Config`
//! section and key.
//!
//! Mechanism: walk the JSON schema of [`thegn_core::config::Config`]
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

mod common;
use common::{Required, section_matches, walk};
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
    // `[project.<slug>.merge_queue]` is the same shape again: an all-Option
    // overlay mirror of `[merge_queue]`, letting one repo refine its gate,
    // target branch and lockfile set. Every key is documented once at its
    // canonical `[merge_queue]` location, and the example shows the overlay
    // pattern there with a representative subset.
    "project.*.merge_queue",
    // `[project.<slug>.pr_queue]` is the same overlay-mirror shape as the
    // merge-queue one above: every key documented once at `[pr_queue]`, with the
    // overlay pattern shown there.
    "project.*.pr_queue",
    // `[project.<slug>.git]` is likewise an all-Option overlay mirror of the
    // base `[git]` table (signing/fetch/diff-view policy per repo). Every key is
    // documented once at its canonical `[git]` location, and the example shows
    // the overlay pattern (`[project.acme.git]`) with a representative subset.
    "project.*.git",
    // `[profiles.<name>.mcp_serve]` / `[project.<slug>.mcp_serve]` are
    // all-Option overlay mirrors of `[mcp.serve]` (the clamp-only MCP-serve
    // scope ceiling); every key (`scopes`) is documented once at its canonical
    // `[mcp.serve]` location, which also explains the overlay clamping pattern.
    "profiles.*.mcp_serve",
    "project.*.mcp_serve",
    // `[host.<n>] capacity` reuses the `ResourcesDecl` shape for its cpu/memory
    // declaration; the ceiling fields (cpu_max/memory_max) are container-limit
    // knobs that have no meaning for a host spec and are never consulted there.
    "host.*.capacity.cpu_max",
    "host.*.capacity.memory_max",
];

fn example_path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/config.toml.example")
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
    let schema = schemars::schema_for!(thegn_core::config::Config);
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
    let cfg: Result<thegn_core::config::Config, _> = toml::from_str(&text);
    if let Err(e) = cfg {
        panic!("config.toml.example does not parse as Config: {e}");
    }
}

/// Values that intentionally illustrate an example instead of the schema
/// default. Each entry must name a schema-defaulted field and its comment must
/// say that it is an example or optional value.
const ILLUSTRATIVE_PLUGIN_VALUES: &[(&str, &str)] = &[
    // RUST_LOG demonstrates pass-through environment configuration.
    ("plugins", "env"),
    // The source label gives the sample status contribution useful metadata.
    ("plugins.contributions", "metadata"),
];

fn plugin_schema_properties(schema: &Value) -> &serde_json::Map<String, Value> {
    schema["properties"]
        .as_object()
        .expect("plugin schema has properties")
}

#[test]
fn plugin_example_block_agrees_with_the_schema() {
    let path = example_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let lines = text.lines().collect::<Vec<_>>();
    let start = lines
        .iter()
        .position(|line| *line == "# [[plugins]]")
        .expect("plugin example block is present");
    let block = lines[start..]
        .iter()
        .take_while(|line| line.starts_with('#'))
        .map(|line| {
            line.strip_prefix("# ")
                .or_else(|| line.strip_prefix('#'))
                .unwrap()
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut example_keys = BTreeSet::new();
    let mut comments = std::collections::BTreeMap::new();
    let mut table = "plugins";
    for line in block.lines() {
        let trimmed = line.trim();
        if trimmed == "[[plugins.contributions]]" {
            table = "plugins.contributions";
            continue;
        }
        let Some((assignment, comment)) = trimmed.split_once('#') else {
            continue;
        };
        let Some((key, _value)) = assignment.split_once('=') else {
            continue;
        };
        let key = key.trim();
        example_keys.insert((table, key));
        comments.insert((table, key), comment.trim());
    }

    let config: thegn_core::config::Config =
        toml::from_str(&block).expect("commented plugin example deserializes as Config");
    assert_eq!(config.plugins.len(), 1);
    let spec = &config.plugins[0];
    assert_eq!(spec.manifest.contributions.len(), 1);

    let contribution = &spec.manifest.contributions[0];
    assert_eq!(spec.manifest.api, thegn_core::plugin_api::API_VERSION);

    let encoded = serde_json::to_value(spec).expect("plugin spec serializes to JSON");
    let decoded: thegn_core::plugin_api::PluginSpec =
        serde_json::from_value(encoded).expect("plugin spec deserializes from JSON");
    assert_eq!(decoded, *spec, "plugin spec JSON round trip is lossless");

    let plugin_schema =
        serde_json::to_value(schemars::schema_for!(thegn_core::plugin_api::PluginSpec))
            .expect("plugin schema serializes");
    let contribution_schema =
        serde_json::to_value(schemars::schema_for!(thegn_core::plugin_api::Contribution))
            .expect("contribution schema serializes");
    let plugin_properties = plugin_schema_properties(&plugin_schema);
    let contribution_properties = plugin_schema_properties(&contribution_schema);
    for (table, key) in ILLUSTRATIVE_PLUGIN_VALUES {
        assert!(
            matches!(*table, "plugins" | "plugins.contributions"),
            "stale illustrative plugin table {table}.{key}"
        );
    }

    for (table, schema, properties) in [
        ("plugins", &plugin_schema, plugin_properties),
        (
            "plugins.contributions",
            &contribution_schema,
            contribution_properties,
        ),
    ] {
        let mut expected = properties
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        if table == "plugins" {
            expected.remove("contributions");
        }
        let actual = example_keys
            .iter()
            .filter_map(|(found_table, key)| (*found_table == table).then_some(*key))
            .collect::<BTreeSet<_>>();
        let missing = expected.difference(&actual).copied().collect::<Vec<_>>();
        let unknown = actual.difference(&expected).copied().collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "example is missing {table} schema keys: {missing:?}"
        );
        assert!(
            unknown.is_empty(),
            "example documents unknown {table} key: {unknown:?}"
        );

        let required = schema["required"]
            .as_array()
            .expect("plugin schema has required list")
            .iter()
            .filter_map(Value::as_str)
            .collect::<BTreeSet<_>>();
        let serialized = if table == "plugins" {
            serde_json::to_value(spec).expect("plugin spec serializes")
        } else {
            serde_json::to_value(contribution).expect("contribution serializes")
        };
        for key in &expected {
            let comment = comments
                .get(&(table, *key))
                .unwrap_or_else(|| panic!("example key {table}.{key} has no trailing comment"));
            assert_eq!(
                comment.contains("(required)"),
                required.contains(key),
                "required marker for {table}.{key} must match schema"
            );
            let illustrative = ILLUSTRATIVE_PLUGIN_VALUES.contains(&(table, *key));
            if let Some(default) = properties[*key].get("default")
                && illustrative
            {
                assert!(
                    comment.contains("default") || comment.contains("Optional"),
                    "illustrative {table}.{key} comment must identify its value as default or Optional"
                );
            } else if let Some(default) = properties[*key].get("default") {
                assert_eq!(
                    serialized.get(*key),
                    Some(default),
                    "{table}.{key} must equal its schema default"
                );
            } else {
                assert!(
                    !illustrative,
                    "illustrative entry {table}.{key} has no schema default"
                );
                if !required.contains(key) {
                    assert!(
                        comment.contains("Optional"),
                        "non-required, non-defaulted {table}.{key} must say Optional"
                    );
                }
            }
        }
        for (illustrative_table, illustrative_key) in ILLUSTRATIVE_PLUGIN_VALUES {
            if *illustrative_table == table {
                assert!(
                    properties
                        .get(*illustrative_key)
                        .and_then(|property| property.get("default"))
                        .is_some(),
                    "stale illustrative plugin value {illustrative_table}.{illustrative_key}"
                );
            }
        }
    }

    let env = spec.env.keys().collect::<Vec<_>>();
    assert!(env.iter().all(|key| !thegn_core::redact::is_sensitive(key)));
    let metadata = contribution.metadata.keys().collect::<Vec<_>>();
    assert!(
        metadata
            .iter()
            .all(|key| !thegn_core::redact::is_sensitive(key))
    );
    assert!(block.contains("do not put secrets here"));

    let support = thegn_core::plugin_api::HOST_EXTENSION_SUPPORT
        .iter()
        .find(|row| row.extension_point.wire_name() == contribution.extension_point.wire_name())
        .expect("example extension point exists in host support table");
    assert_eq!(support.state, thegn_core::plugin_api::SupportState::Wired);
    assert!(support.cadences.contains(&contribution.cadence.kind()));
    assert!(support.modes.contains(&spec.mode));
}

#[test]
fn generated_config_reference_keeps_example_schema_keys() {
    let path = example_path();
    let example = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    let generated = thegn_core::help::config_ref::page(&example);
    let doc = scan_example(&generated);
    let required = common::required();
    let mut missing = Vec::new();
    let mut expected = BTreeSet::new();
    for (pattern, key) in &required.keys {
        if allowlisted(pattern, Some(key)) {
            continue;
        }
        expected.insert(key);
    }
    let generated_keys: BTreeSet<&String> = doc.keys.iter().map(|(_, key)| key).collect();
    for key in expected {
        if !generated_keys.contains(key) {
            missing.push(key.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "generated config reference dropped schema/example keys:\n  {}",
        missing.join("\n  ")
    );
}
