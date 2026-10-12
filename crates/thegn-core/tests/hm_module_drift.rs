//! `nix/hm-module.nix` is a hand-maintained third copy of the config schema
//! (Rust structs → `config.toml.example` → the home-manager module).
//! Four drift checks keep it honest against the Rust source of truth:
//!
//! 1. every TOML key the module's `tomlFormat.generate` block renders exists
//!    in the `Config` schema (same walker as the example-file test);
//! 2. every `lib.types.enum [...]` value list is a subset of some
//!    `config_enum!`'s accepted spellings (canonical + aliases), so the Nix
//!    option can never offer a value the binary rejects.
//! 3. every numeric option's Nix range matches its Rust schema destination.
//! 4. every Nix enum's values are accepted by that option's exact Rust schema
//!    destination.
//! 5. every schema root and each partially-rendered section has an explicit
//!    rendered or omitted disposition.
//!
//! The mapping is deliberately parsed from the Nix text (no Nix evaluator in
//! the test): the block is a flat attrset of `snake_key = …;`, `section.key
//! = …;`, nested `section = { … };` and `inherit (cfg.x) a b c;` lines.

mod common;

use serde_json::Value;
use std::collections::BTreeSet;

/// Intentional Home Manager omissions. Root entries cover wholly unexposed
/// families; the partially-rendered families use key paths. `sandbox` remains
/// one root disposition while THE-419 settles safe defaults.
const OMITTED: &[(&str, &str)] = &[
    ("window_margin", "terminal geometry is host-specific"),
    ("default_folder", "worktree filing policy is not exposed"),
    ("confirm_delete", "destructive-action policy is not exposed"),
    ("profile", "profile selection is host-specific"),
    ("keymap_preset", "keymap presets are host-specific"),
    ("accounts", "coding-agent accounts are not exposed"),
    ("pins", "workspace pin state is not exposed"),
    ("tasks", "task definitions are not exposed"),
    ("worktree_templates", "worktree templates are not exposed"),
    ("presets", "launch presets are not exposed"),
    ("git_commands", "custom git commands are not exposed"),
    ("plugins", "plugin configuration is not exposed"),
    ("forges", "forge connections are not exposed"),
    ("ui", "UI behavior is not exposed"),
    ("git", "Git behavior is not exposed"),
    ("editor", "editor integration is not exposed"),
    ("stats", "resource sampling is not exposed"),
    ("metrics", "metrics export is not exposed"),
    ("apps", "app ordering is not exposed"),
    ("observe", "Observe configuration is not exposed"),
    ("bars", "status bar configuration is not exposed"),
    ("calendar", "calendar sources are not exposed"),
    ("weather", "weather configuration is not exposed"),
    ("issues", "issue providers are not exposed"),
    ("ci", "CI providers are not exposed"),
    ("diagnostics", "diagnostics policy is not exposed"),
    ("database", "database migration policy is not exposed"),
    ("hooks", "lifecycle hooks are not exposed"),
    ("toolchain", "toolchain policy is not exposed"),
    ("limits", "resource limits are not exposed"),
    ("disk", "disk and cache policy is not exposed"),
    ("loc", "lines-of-code display is not exposed"),
    ("session", "session retention is not exposed"),
    ("activity", "activity detection is not exposed"),
    ("drawer", "drawer behavior is not exposed"),
    ("notifications", "notifications are not exposed"),
    ("automations", "automation rules are not exposed"),
    ("strip", "strip behavior is not exposed"),
    ("panel", "panel layout is not exposed"),
    ("search", "search policy is not exposed"),
    ("palette", "palette behavior is not exposed"),
    ("lsp", "language-server configuration is not exposed"),
    ("semantic", "semantic indexing is not exposed"),
    ("daemon", "daemon configuration is not exposed"),
    ("serve", "remote serving policy is not exposed"),
    ("merge_queue", "merge queue policy is not exposed"),
    ("pr_queue", "PR queue policy is not exposed"),
    ("autopilot", "autopilot policy is not exposed"),
    ("pipeline", "pipeline definitions are not exposed"),
    ("skills", "skill configuration is not exposed"),
    ("replay", "pane replay is not exposed"),
    ("recording", "session recording is not exposed"),
    ("clipboard", "clipboard policy is not exposed"),
    ("media", "media controls are not exposed"),
    ("voice", "voice input is not exposed"),
    ("usage", "usage tracking is not exposed"),
    ("model_proxy", "model proxy is not exposed"),
    ("remote", "remote connection tuning is not exposed"),
    ("network", "offline detection is not exposed"),
    ("host_discovery", "host discovery is not exposed"),
    ("share", "port sharing is not exposed"),
    ("forward", "port forwarding is not exposed"),
    ("preview", "preview policy is not exposed"),
    ("lifecycle", "provider lifecycle policy is not exposed"),
    ("placement", "multi-host placement is not exposed"),
    ("profiles", "named profiles are not exposed"),
    ("profile_order", "profile ordering is not exposed"),
    ("identities", "identity configuration is not exposed"),
    ("project", "project overlays are not exposed"),
    ("env", "execution environments are not exposed"),
    ("host", "host definitions are not exposed"),
    ("bundle", "identity bundles are not exposed"),
    ("zone", "zone policy is not exposed"),
    ("managed_tools", "managed tool overrides are not exposed"),
    ("mcp_servers", "MCP server definitions are not exposed"),
    ("mcp_proxy", "MCP proxy policy is not exposed"),
    ("mcp", "MCP endpoint policy is not exposed"),
    ("secrets", "secret resolver configuration is not exposed"),
    ("credentials", "credential custody policy is not exposed"),
    (
        "program_keybinds",
        "program-specific keybinds are not exposed",
    ),
    (
        "program_remap",
        "program-specific key remaps are not exposed",
    ),
    ("theme.preset", "theme preset selection is not exposed"),
    ("theme.focus_border", "focus border color is not exposed"),
    ("theme.pane_padding", "pane padding is not exposed"),
    ("theme.undercurl", "undercurl policy is not exposed"),
    ("theme.color", "color fidelity is not exposed"),
    ("theme.glyphs", "glyph fidelity is not exposed"),
    ("theme.agent_glyphs", "agent glyph style is not exposed"),
    ("theme.mascot", "splash mascot is not exposed"),
    ("theme.mascot_motion", "splash mascot motion is not exposed"),
    ("theme.colors", "surface palette overrides are not exposed"),
    ("theme.hues", "semantic hue overrides are not exposed"),
    (
        "monitor.window_ladder",
        "monitor graph windows are not exposed",
    ),
    (
        "monitor.default_window",
        "monitor starting window is not exposed",
    ),
    (
        "monitor.default_style",
        "monitor graph style is not exposed",
    ),
    (
        "monitor.default_scale",
        "monitor graph scale is not exposed",
    ),
    (
        "monitor.proc_rows",
        "monitor process row count is not exposed",
    ),
    ("monitor.processes", "process sampling is not exposed"),
    ("monitor.process_io", "process I/O sampling is not exposed"),
    (
        "log.stderr_cap_mb",
        "captured stderr retention is not exposed",
    ),
    ("sandbox", "blocked on THE-419 safe defaults"),
];

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

/// Option names and their declared Nix type expressions for scalar numeric
/// options. The destination map below pins each name to its Rust schema path.
fn numeric_options(src: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(expr) = line
            .trim()
            .strip_prefix("type = ")
            .map(|s| s.trim_end_matches(';'))
        else {
            continue;
        };
        if !(expr.starts_with("lib.types.int") || expr == "lib.types.port") {
            continue;
        }
        let name = lines[..i]
            .iter()
            .rev()
            .find_map(|previous| {
                previous
                    .trim()
                    .strip_suffix("= lib.mkOption {")
                    .map(|n| n.trim().to_string())
            })
            .unwrap_or_else(|| format!("numeric@{}", i + 1));
        out.push((name, expr.to_string()));
    }
    out
}

fn schema_field(path: &str) -> Value {
    let schema = schemars::schema_for!(thegn_core::config::Config);
    let root = serde_json::to_value(&schema).expect("schema to json");
    let defs = root["definitions"].as_object().expect("schema definitions");
    let mut current = &root;
    for segment in path.split('.') {
        current = common::resolve(defs, current)
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(segment))
            .unwrap_or_else(|| panic!("schema path `{path}` is missing segment `{segment}`"));
    }
    common::resolve(defs, current).clone()
}

fn schema_has_path(path: &str) -> bool {
    let schema = schemars::schema_for!(thegn_core::config::Config);
    let root = serde_json::to_value(&schema).expect("schema to json");
    let defs = root["definitions"].as_object().expect("schema definitions");
    let mut current = &root;
    for segment in path.split('.') {
        let resolved = common::resolve(defs, current);
        let Some(next) = resolved
            .get("properties")
            .and_then(Value::as_object)
            .and_then(|properties| properties.get(segment))
        else {
            return false;
        };
        current = next;
    }
    true
}

fn path_is_rendered(path: &str, rendered: &BTreeSet<String>) -> bool {
    rendered
        .iter()
        .any(|candidate| candidate == path || candidate.starts_with(&format!("{path}.")))
}

#[test]
fn every_schema_key_is_rendered_or_dispositioned() {
    let rendered = rendered_paths(&module_src());
    let schema = schemars::schema_for!(thegn_core::config::Config);
    let root = serde_json::to_value(&schema).expect("schema to json");
    let root_props: BTreeSet<String> = root["properties"]
        .as_object()
        .expect("Config schema properties")
        .keys()
        .cloned()
        .collect();
    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();

    let mut missing = Vec::new();
    for root_key in &root_props {
        if !path_is_rendered(root_key, &rendered) && !omissions.contains(root_key.as_str()) {
            missing.push(root_key.clone());
        }
    }

    let required = common::required();
    for section in ["theme", "monitor", "pr", "watch", "log", "sandbox.remote"] {
        for (schema_section, key) in &required.keys {
            let path = if schema_section.is_empty() {
                key.clone()
            } else {
                format!("{schema_section}.{key}")
            };
            if (schema_section == section || schema_section.starts_with(&format!("{section}.")))
                && !path_is_rendered(&path, &rendered)
                && !omissions
                    .iter()
                    .any(|omitted| path == *omitted || path.starts_with(&format!("{omitted}.")))
            {
                missing.push(path);
            }
        }
    }

    let mut stale = Vec::new();
    for (omitted, rationale) in OMITTED {
        if rationale.trim().is_empty() || !schema_has_path(omitted) {
            stale.push((*omitted).to_string());
            continue;
        }
        let still_omitted = if *omitted == "sandbox" {
            required.keys.iter().any(|(section, key)| {
                let path = if section.is_empty() {
                    key.clone()
                } else {
                    format!("{section}.{key}")
                };
                path.starts_with("sandbox.") && !path_is_rendered(&path, &rendered)
            })
        } else {
            !path_is_rendered(omitted, &rendered)
        };
        if !still_omitted {
            stale.push((*omitted).to_string());
        }
    }
    missing.sort();
    stale.sort();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "Home Manager schema disposition drift; missing={missing:?}; stale={stale:?}"
    );
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

#[test]
fn every_numeric_option_matches_its_destination_range() {
    let expected = [
        ("rotationSizeMb", "log.rotation_size_mb"),
        ("maxFiles", "log.max_files"),
        ("repoScanDepth", "repo_scan_depth"),
        ("prTtlSecs", "pr.ttl_secs"),
        ("watchPrIntervalSecs", "watch.pr_interval_secs"),
        ("port", "sandbox.remote.port"),
    ];
    let options = numeric_options(&module_src());
    let actual_names: BTreeSet<&str> = options.iter().map(|(name, _)| name.as_str()).collect();
    let expected_names: BTreeSet<&str> = expected.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        actual_names, expected_names,
        "every Nix integer/port type must have an explicit Rust schema destination"
    );

    for (name, nix_type) in options {
        let path = expected
            .iter()
            .find_map(|(option, path)| (*option == name).then_some(*path))
            .expect("numeric option destination");
        let schema = schema_field(path);
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("integer"),
            "schema type for {path}"
        );
        if name == "port" {
            assert_eq!(schema.get("format").and_then(Value::as_str), Some("uint16"));
            assert_eq!(nix_type, "lib.types.port", "Nix type for {path}");
            continue;
        }

        let minimum = schema.get("minimum").and_then(Value::as_f64);
        assert_eq!(
            minimum,
            Some(0.0),
            "schema minimum for unsigned field {path}"
        );
        let maximum = schema.get("maximum").and_then(Value::as_f64).map(|m| {
            assert!(
                m.fract().abs() < f64::EPSILON,
                "schema maximum for {path} must be a whole number, got {m}"
            );
            m as u64
        });
        if let Some(maximum) = maximum {
            assert_eq!(
                nix_type,
                format!("lib.types.ints.between 0 {maximum}"),
                "Nix type for bounded schema field {path}"
            );
        } else {
            assert_eq!(
                nix_type, "lib.types.ints.unsigned",
                "Nix type for unsigned schema field {path}"
            );
        }
    }
}

#[test]
fn every_nix_enum_matches_its_destination_enum() {
    let expected = [
        ("transport", "sandbox.remote.transport"),
        ("mode", "sandbox.remote.mode"),
        ("backend", "sandbox.backend"),
        ("network", "sandbox.network"),
        ("onMissing", "sandbox.on_missing"),
        ("level", "log.level"),
        ("format", "log.format"),
        ("picker", "picker"),
        ("worktreeMode", "worktree_mode"),
        ("nameScheme", "name_scheme"),
    ];
    let enums = enum_lists(&module_src());
    let actual_names: BTreeSet<&str> = enums.iter().map(|(name, _)| name.as_str()).collect();
    let expected_names: BTreeSet<&str> = expected.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        actual_names, expected_names,
        "every Nix enum must have an explicit Rust schema destination"
    );

    for (name, values) in enums {
        let path = expected
            .iter()
            .find_map(|(option, path)| (*option == name).then_some(*path))
            .expect("enum option destination");
        let schema = schema_field(path);
        let mut accepted: BTreeSet<String> = schema["enum"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if let Some(marker) = schema.get(thegn_core::config_validate::ENUM_MARKER) {
            accepted.extend(
                marker["aliases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string),
            );
        }
        assert!(
            values.iter().all(|value| accepted.contains(value)),
            "Nix enum option `{name}` offers values outside `{path}`: {:?}",
            values
                .iter()
                .filter(|value| !accepted.contains(*value))
                .collect::<Vec<_>>()
        );
    }
}
