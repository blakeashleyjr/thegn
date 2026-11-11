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
    (
        "git.backend",
        "the git read-engine enum needs destination pinning and is not exposed",
    ),
    (
        "git.structural_diff",
        "the structural-diff enum needs destination pinning and is not exposed",
    ),
    (
        "git.submodules",
        "the submodule-mode enum needs destination pinning and is not exposed",
    ),
    ("editor", "editor integration is not exposed"),
    (
        "stats.refresh_secs",
        "refresh interval is an f64 range that needs float pinning and is not exposed",
    ),
    (
        "stats.refresh_rates",
        "refresh-rate list needs float list pinning and is not exposed",
    ),
    (
        "stats.battery_warn",
        "u8 threshold needs byte-range pinning and is not exposed",
    ),
    (
        "stats.disk_free_warn",
        "u8 threshold needs byte-range pinning and is not exposed",
    ),
    (
        "stats.disk_free_critical",
        "u8 threshold needs byte-range pinning and is not exposed",
    ),
    (
        "stats.alerts",
        "threshold alerts are a nested table and are not exposed",
    ),
    (
        "stats.net_icons",
        "per-medium network glyph table is not exposed",
    ),
    ("metrics", "metrics export is not exposed"),
    ("apps", "app ordering is not exposed"),
    ("observe", "Observe configuration is not exposed"),
    ("bars", "status bar configuration is not exposed"),
    ("calendar", "calendar sources are not exposed"),
    ("weather", "weather configuration is not exposed"),
    ("issues", "issue providers are not exposed"),
    ("ci", "CI providers are not exposed"),
    (
        "diagnostics.crash_sink",
        "reserved: any non-empty value is rejected at load",
    ),
    ("database", "database migration policy is not exposed"),
    ("hooks", "lifecycle hooks are not exposed"),
    ("toolchain", "toolchain policy is not exposed"),
    (
        "limits.test_nice",
        "signed i32: the numeric gate models only unsigned ranges",
    ),
    (
        "disk.sccache_dir",
        "path-valued shared-cache directory is not exposed",
    ),
    (
        "disk.shared_target_dir",
        "path-valued shared target directory is deliberately opt-in and is not exposed",
    ),
    (
        "notifications.desktop_min_urgency",
        "string-typed urgency has no destination enum to pin and is not exposed",
    ),
    (
        "notifications.process_exit",
        "string-typed exit policy has no destination enum to pin and is not exposed",
    ),
    (
        "notifications.agent_error_signatures",
        "bounded validated signature list is not exposed",
    ),
    (
        "notifications.priority",
        "per-kind priority map is not exposed",
    ),
    ("notifications.rules", "routing rules are not exposed"),
    ("notifications.dnd.windows", "window list is not exposed"),
    (
        "notifications.dnd.allow_priority",
        "priority string has no pinned destination enum here",
    ),
    (
        "notifications.sound.mode",
        "sound mode enum is not exposed yet",
    ),
    (
        "notifications.sound.min_priority",
        "sound priority string has no pinned destination enum here",
    ),
    (
        "notifications.sound.always_kinds",
        "sound kind list is not exposed yet",
    ),
    (
        "notifications.sound.pack",
        "sound pack path is not exposed yet",
    ),
    (
        "notifications.sound.volume",
        "sound volume float is not exposed yet",
    ),
    (
        "notifications.sound.chime_file",
        "sound chime path is not exposed yet",
    ),
    (
        "notifications.sound.command",
        "sound command is executable-bearing and is not exposed yet",
    ),
    (
        "notifications.sound.per_priority",
        "per-priority sound map is not exposed yet",
    ),
    (
        "notifications.sound.per_kind",
        "per-kind sound map is not exposed yet",
    ),
    ("notifications.modes", "notification modes are not exposed"),
    (
        "notifications.active_mode",
        "active mode selection is not exposed",
    ),
    (
        "notifications.push",
        "push sinks carry credentials and are not exposed",
    ),
    (
        "activity.cpu_percent",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.runaway_core_fraction",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.runaway_secs",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.quiet_grace_secs",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.resume_grace_secs",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.spawn_grace_secs",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.unsolicited_gap_secs",
        "f64 threshold needs float pinning and is not exposed",
    ),
    (
        "activity.output_hint_ttl_secs",
        "f64 threshold needs float pinning and is not exposed",
    ),
    ("automations", "automation rules are not exposed"),
    (
        "drawer.kind",
        "reserved file-manager kinds need enum pinning and are not exposed",
    ),
    (
        "strip.ratio",
        "strip ratio is an f32 range that needs float pinning and is not exposed",
    ),
    ("panel", "panel layout is not exposed"),
    ("lsp", "language-server configuration is not exposed"),
    ("semantic", "semantic indexing is not exposed"),
    ("daemon", "daemon configuration is not exposed"),
    ("serve", "remote serving policy is not exposed"),
    ("merge_queue", "merge queue policy is not exposed"),
    ("pr_queue", "PR queue policy is not exposed"),
    ("autopilot", "autopilot policy is not exposed"),
    ("pipeline", "pipeline definitions are not exposed"),
    ("skills", "skill configuration is not exposed"),
    (
        "media.backend",
        "media backend enum needs destination pinning and is not exposed",
    ),
    (
        "media.default_action",
        "media default-action enum needs destination pinning and is not exposed",
    ),
    (
        "media.volume_step",
        "media volume step is a float and needs float pinning",
    ),
    ("media.mpv", "mpv backend table is not exposed"),
    ("media.mpd", "mpd backend table is not exposed"),
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
    ("theme.colors", "surface palette overrides are not exposed"),
    ("theme.hues", "semantic hue overrides are not exposed"),
    (
        "ui.sidebar_width",
        "sidebar width needs optional-number range pinning and is not exposed",
    ),
    (
        "ui.sidebar_wide_ratio",
        "sidebar wide-expand ratio needs optional-number range pinning and is not exposed",
    ),
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
        "log.stderr_cap_mb",
        "captured stderr retention is not exposed",
    ),
    (
        "sandbox",
        "only part of [sandbox] is exposed; unset options fall through to core defaults",
    ),
];

fn module_src() -> String {
    std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../nix/hm-module.nix"),
    )
    .expect("nix/hm-module.nix")
}

fn declared_default(line: &str) -> Option<&str> {
    line.strip_prefix("default = ")
        .map(|value| value.strip_suffix(';').unwrap_or(value).trim())
}

fn strip_null_or(expr: &str) -> &str {
    let Some(inner) = expr.strip_prefix("lib.types.nullOr ") else {
        return expr;
    };
    let inner = inner.trim();
    if !inner.starts_with('(') || !inner.ends_with(')') {
        return inner;
    }
    let mut depth = 0usize;
    for (index, character) in inner.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && index + character.len_utf8() != inner.len() {
                    return inner;
                }
            }
            _ => {}
        }
    }
    if depth == 0 {
        &inner[1..inner.len() - 1]
    } else {
        inner
    }
}

#[test]
fn every_value_option_defaults_to_null() {
    let src = module_src();
    let item_ranges = [
        (
            src.find("  agentSubmodule =")
                .expect("agentSubmodule declaration"),
            src.find("  remoteSubmodule =")
                .expect("remoteSubmodule declaration"),
        ),
        (
            src.find("  actionSubmodule =")
                .expect("actionSubmodule declaration"),
            src.find("  tomlFormat =").expect("tomlFormat declaration"),
        ),
    ];
    let mut offenders = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    let mut offset = 0usize;
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        let Some(name) = trimmed.strip_suffix("= lib.mkOption {") else {
            offset += line.len() + 1;
            index += 1;
            continue;
        };
        let name = name.trim();
        let declaration_offset = offset;
        let indentation = line.len() - line.trim_start().len();
        offset += line.len() + 1;
        index += 1;
        let mut default = None;
        while index < lines.len() {
            let option_line = lines[index];
            let option_trimmed = option_line.trim();
            if option_line.len() - option_line.trim_start().len() == indentation
                && option_trimmed == "};"
            {
                offset += option_line.len() + 1;
                index += 1;
                break;
            }
            if let Some(value) = declared_default(option_trimmed) {
                default = Some(value);
            }
            offset += option_line.len() + 1;
            index += 1;
        }
        let Some(value) = default else {
            continue;
        };
        if value == "null" {
            continue;
        }
        let is_list_item_field = item_ranges
            .iter()
            .any(|(start, end)| *start <= declaration_offset && declaration_offset < *end);
        let is_submodule_container = matches!(name, "package" | "log" | "sandbox")
            || (name == "remote"
                && src[..declaration_offset].rfind("sandboxSubmodule =")
                    > src[..declaration_offset].rfind("remoteSubmodule ="));
        if !is_list_item_field && !is_submodule_container {
            offenders.push(format!("{name}: {value}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "value options must default to null (allowed submodule containers and list-item fields excluded): {offenders:?}"
    );
}

#[test]
fn multiline_default_opening_line_is_recorded() {
    assert_eq!(declared_default("default = ["), Some("["));
    assert_eq!(declared_default("default = null;"), Some("null"));
}

#[test]
fn generate_block_prunes_nulls() {
    let src = module_src();
    assert!(
        src.contains("tomlFormat.generate \"thegn-config.toml\" (pruneEmpty (pruneNulls {"),
        "generated config must recursively prune null values and empty tables"
    );
    assert!(
        src.contains("pruneNulls = lib.filterAttrsRecursive (_: v: v != null);"),
        "the recursive prune predicate must remove null values"
    );
}

#[test]
fn generate_block_prunes_empty_tables_for_all_null_defaults() {
    let src = module_src();
    assert!(
        src.contains("pruneEmpty = value:")
            && src.contains("lib.mapAttrs (_: pruneEmpty) value")
            && src.contains("lib.filterAttrs (_: v: v != {}) pruned"),
        "empty attrsets must be recursively pruned after null values"
    );
    assert!(
        src.contains("(pruneEmpty (pruneNulls {")
            && src.contains("theme.accent = cfg.themeAccent;")
            && src.contains("remote = {"),
        "the all-null render must pass its nested config tables through pruneEmpty"
    );
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
    let lines: Vec<&str> = block.lines().collect();
    let mut index = 0usize;
    while index < lines.len() {
        let mut line = lines[index].trim().to_string();
        index += 1;
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A Nix assignment may put its `if/then/else` expression on the next
        // lines. Collect through the terminating semicolon before inspecting
        // map/inherit expressions. These continuation keywords are never
        // assignments even if their text contains `=` (e.g. `if x == null`).
        if line.starts_with("if ") || line.starts_with("then ") || line.starts_with("else ") {
            continue;
        }
        if line.ends_with('=') {
            while index < lines.len() {
                let continuation = lines[index].trim();
                line.push(' ');
                line.push_str(continuation);
                index += 1;
                if continuation.ends_with(';') {
                    break;
                }
            }
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
                // under `lhs`. Guarded maps use `if ... then null else map ...`.
                if let Some(map_start) = rhs.find("map (") {
                    let inner = &rhs[map_start + "map (".len()..];
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
    assert!(
        out.contains("agents.name") && out.contains("actions.hint") && out.contains("projects_dir"),
        "conditional assignments and list item keys must remain visible: {out:?}"
    );
    out
}

/// `lib.types.enum ["a" "b"]` value lists, with the option name they belong to.
fn enum_lists(src: &str) -> Vec<(String, Vec<String>)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = src.lines().collect();
    for (i, l) in lines.iter().enumerate() {
        let Some(expr) = l
            .trim()
            .strip_prefix("type = ")
            .map(|expr| expr.trim_end_matches(';'))
        else {
            continue;
        };
        if let Some(rest) = strip_null_or(expr).strip_prefix("lib.types.enum [") {
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
        let expr = strip_null_or(expr);
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

fn is_dispositioned(path: &str, section: &str, omissions: &BTreeSet<&str>) -> bool {
    omissions.iter().any(|omitted| {
        let omitted_in_section = *omitted == section || omitted.starts_with(&format!("{section}."));
        omitted_in_section && (path == *omitted || path.starts_with(&format!("{omitted}.")))
    })
}

#[test]
fn section_disposition_ignores_ancestor_omissions() {
    let omissions = BTreeSet::from(["sandbox"]);
    assert!(!is_dispositioned(
        "sandbox.remote.new_key",
        "sandbox.remote",
        &omissions,
    ));
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
    for section in [
        "theme",
        "ui",
        "search",
        "palette",
        "session",
        "clipboard",
        "loc",
        "replay",
        "drawer",
        "strip",
        "git",
        "stats",
        "notifications",
        "activity",
        "monitor",
        "recording",
        "media",
        "pr",
        "watch",
        "log",
        "limits",
        "diagnostics",
        "disk",
        "sandbox.remote",
    ] {
        for (schema_section, key) in &required.keys {
            let path = if schema_section.is_empty() {
                key.clone()
            } else {
                format!("{schema_section}.{key}")
            };
            if (schema_section == section || schema_section.starts_with(&format!("{section}.")))
                && !path_is_rendered(&path, &rendered)
                && !is_dispositioned(&path, section, &omissions)
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

#[test]
fn hm_theme_family_is_fully_rendered() {
    let theme = schema_field("theme");
    let properties = theme["properties"]
        .as_object()
        .expect("theme schema properties");
    let rendered = rendered_paths(&module_src());
    let mut missing: Vec<String> = properties
        .keys()
        .filter(|key| !matches!(key.as_str(), "colors" | "hues"))
        .filter_map(|key| {
            let path = format!("theme.{key}");
            (!rendered.contains(&path)).then_some(path)
        })
        .collect();
    if !rendered.contains("confirm_delete") {
        missing.push("confirm_delete".to_string());
    }
    assert!(
        missing.is_empty(),
        "Home Manager must render every scalar theme field and confirm_delete; missing: {missing:?}"
    );
}

#[test]
fn hm_theme_enum_options_offer_only_canonical_spellings() {
    let expected = [
        ("themeUndercurl", "theme.undercurl"),
        ("themeColor", "theme.color"),
        ("themeGlyphs", "theme.glyphs"),
        ("themeAgentGlyphs", "theme.agent_glyphs"),
        ("themeMascot", "theme.mascot"),
        ("themeMascotMotion", "theme.mascot_motion"),
    ];
    let enums = enum_lists(&module_src());
    for (option, path) in expected {
        let values = enums
            .iter()
            .find(|(name, _)| name == option)
            .map(|(_, values)| values)
            .unwrap_or_else(|| panic!("Home Manager enum option `{option}` is missing"));
        let actual: BTreeSet<&str> = values.iter().map(String::as_str).collect();
        let schema = schema_field(path);
        let canonical: BTreeSet<&str> = schema["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("destination `{path}` has no enum"))
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            actual, canonical,
            "Home Manager enum option `{option}` must expose exactly canonical values for `{path}`"
        );
    }
}

#[test]
fn hm_ui_family_is_fully_rendered() {
    let ui = schema_field("ui");
    let properties = ui["properties"].as_object().expect("ui schema properties");
    let rendered = rendered_paths(&module_src());
    let mut missing: Vec<String> = properties
        .keys()
        .filter(|key| !matches!(key.as_str(), "sidebar_width" | "sidebar_wide_ratio"))
        .filter_map(|key| {
            let path = format!("ui.{key}");
            (!rendered.contains(&path)).then_some(path)
        })
        .collect();
    missing.sort();
    assert!(
        missing.is_empty(),
        "Home Manager must render every exposed UI scalar; missing: {missing:?}"
    );
}

#[test]
fn hm_ui_enum_options_offer_only_canonical_spellings() {
    let expected = [
        ("uiSidebarProjectSort", "ui.sidebar_project_sort"),
        ("uiSidebarTerminalsSection", "ui.sidebar_terminals_section"),
        ("uiSidebarFocusDetail", "ui.sidebar_focus_detail"),
    ];
    let enums = enum_lists(&module_src());
    for (option, path) in expected {
        let values = enums
            .iter()
            .find(|(name, _)| name == option)
            .map(|(_, values)| values)
            .unwrap_or_else(|| panic!("Home Manager enum option `{option}` is missing"));
        let actual: BTreeSet<&str> = values.iter().map(String::as_str).collect();
        let schema = schema_field(path);
        let canonical: BTreeSet<&str> = schema["enum"]
            .as_array()
            .unwrap_or_else(|| panic!("destination `{path}` has no enum"))
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(
            actual, canonical,
            "Home Manager enum option `{option}` must expose exactly canonical values for `{path}`"
        );
    }
}

#[test]
fn hm_search_and_palette_families_are_fully_rendered() {
    let rendered = rendered_paths(&module_src());
    for table in ["search", "palette"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        let mut missing: Vec<String> = properties
            .keys()
            .filter_map(|key| {
                let path = format!("{table}.{key}");
                (!rendered.contains(&path)).then_some(path)
            })
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "Home Manager must render every {table} scalar; missing: {missing:?}"
        );
    }
}

#[test]
fn hm_search_structural_offers_only_implemented_spellings() {
    let enums = enum_lists(&module_src());
    let values = enums
        .iter()
        .find(|(name, _)| name == "searchStructural")
        .map(|(_, values)| values)
        .unwrap_or_else(|| panic!("Home Manager enum option `searchStructural` is missing"));
    let actual: BTreeSet<&str> = values.iter().map(String::as_str).collect();
    let schema = schema_field("search.structural");
    let mut implemented: BTreeSet<&str> = schema["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("destination `search.structural` has no enum"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if let Some(marker) = schema.get(thegn_core::config_validate::ENUM_MARKER) {
        for reserved in marker["reserved"].as_array().into_iter().flatten() {
            if let Some(spelling) = reserved.as_str() {
                implemented.remove(spelling);
            }
        }
    }
    assert!(
        !implemented.is_empty(),
        "search.structural must retain at least one implemented spelling"
    );
    assert_eq!(
        actual, implemented,
        "Home Manager searchStructural must offer exactly the implemented spellings"
    );
}

#[test]
fn hm_session_and_clipboard_families_are_fully_rendered() {
    let rendered = rendered_paths(&module_src());
    for table in ["session", "clipboard"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        let mut missing: Vec<String> = properties
            .keys()
            .filter_map(|key| {
                let path = format!("{table}.{key}");
                (!rendered.contains(&path)).then_some(path)
            })
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "Home Manager must render every {table} scalar; missing: {missing:?}"
        );
    }
}

#[test]
fn hm_drawer_and_strip_families_are_fully_rendered() {
    let rendered = rendered_paths(&module_src());
    for (table, omitted_key) in [("drawer", "kind"), ("strip", "ratio")] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        let mut missing: Vec<String> = properties
            .keys()
            .filter_map(|key| {
                if key == omitted_key {
                    return None;
                }
                let path = format!("{table}.{key}");
                (!rendered.contains(&path)).then_some(path)
            })
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "Home Manager must render every {table} scalar except {omitted_key}; missing: {missing:?}"
        );
    }
    assert!(schema_has_path("drawer.kind"));
    assert!(schema_has_path("strip.ratio"));
}

#[test]
fn hm_git_and_stats_families_are_rendered() {
    let rendered = rendered_paths(&module_src());
    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["git", "stats"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            let path = format!("{table}.{key}");
            if omissions.contains(path.as_str()) {
                assert!(
                    schema_has_path(&path),
                    "omitted schema key must exist: {path}"
                );
            } else {
                assert!(rendered.contains(&path), "Home Manager must render {path}");
            }
        }
    }
    for path in [
        "git.backend",
        "git.structural_diff",
        "git.submodules",
        "stats.refresh_secs",
        "stats.refresh_rates",
        "stats.battery_warn",
        "stats.disk_free_warn",
        "stats.disk_free_critical",
        "stats.alerts",
        "stats.net_icons",
    ] {
        assert!(schema_has_path(path), "omission must stay pinned: {path}");
    }
}

#[test]
fn hm_notifications_and_activity_families_are_rendered() {
    let rendered = rendered_paths(&module_src());
    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["notifications", "activity"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            let path = format!("{table}.{key}");
            if omissions.contains(path.as_str()) {
                assert!(
                    schema_has_path(&path),
                    "omitted schema key must exist: {path}"
                );
            } else {
                assert!(
                    rendered.contains(&path) || path_is_rendered(&path, &rendered),
                    "Home Manager must render {path} or its children"
                );
            }
        }
    }
    for path in [
        "notifications.desktop_min_urgency",
        "notifications.process_exit",
        "notifications.agent_error_signatures",
        "notifications.priority",
        "notifications.rules",
        "notifications.dnd.windows",
        "notifications.dnd.allow_priority",
        "notifications.sound.mode",
        "notifications.sound.min_priority",
        "notifications.sound.always_kinds",
        "notifications.sound.pack",
        "notifications.sound.volume",
        "notifications.sound.chime_file",
        "notifications.sound.command",
        "notifications.sound.per_priority",
        "notifications.sound.per_kind",
        "notifications.modes",
        "notifications.active_mode",
        "notifications.push",
        "activity.cpu_percent",
        "activity.runaway_core_fraction",
        "activity.runaway_secs",
        "activity.quiet_grace_secs",
        "activity.resume_grace_secs",
        "activity.spawn_grace_secs",
        "activity.unsolicited_gap_secs",
        "activity.output_hint_ttl_secs",
    ] {
        assert!(schema_has_path(path), "omission must stay pinned: {path}");
    }
}

#[test]
fn hm_notifications_dnd_and_sound_bools_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "notifications.dnd.enabled",
        "notifications.sound.mute",
        "notifications.sound.suppress_focused",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["notifications.dnd", "notifications.sound"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            let path = format!("{table}.{key}");
            assert!(
                rendered.contains(&path) || omissions.contains(path.as_str()),
                "{path} must be rendered or dispositioned by an exact ledger row"
            );
        }
    }
}

#[test]
fn hm_recording_media_and_monitor_process_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "recording.dir",
        "recording.max_bytes",
        "media.enabled",
        "media.show_art",
        "media.overlay_on_badge_click",
        "media.seek_step_secs",
        "media.seek_step_video_secs",
        "media.poll_interval_secs",
        "media.players_priority",
        "monitor.proc_rows",
        "monitor.processes",
        "monitor.process_io",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["recording", "media", "monitor"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            let path = format!("{table}.{key}");
            assert!(
                rendered.contains(&path) || omissions.contains(path.as_str()),
                "{path} must be rendered or dispositioned by an exact ledger row"
            );
        }
    }
}

#[test]
fn hm_limits_and_diagnostics_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "limits.test_cpu_quota",
        "limits.test_mem_max",
        "limits.test_max_parallel",
        "limits.test_timeout_secs",
        "limits.discover_timeout_secs",
        "limits.isolated_target_dir",
        "diagnostics.crash_reports",
        "diagnostics.crash_retention",
        "diagnostics.ring_size",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["limits", "diagnostics"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            let path = format!("{table}.{key}");
            assert!(
                rendered.contains(&path) || omissions.contains(path.as_str()),
                "{path} must be rendered or dispositioned by an exact ledger row"
            );
        }
    }
}

#[test]
fn hm_disk_scalar_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "disk.show_sizes",
        "disk.warn_threshold_gb",
        "disk.scan_interval_secs",
        "disk.max_scan_per_round",
        "disk.auto_clean_on_merge",
        "disk.clean_on_pr_closed",
        "disk.idle_clean_days",
        "disk.generation_min_age_days",
        "disk.reclaim_on_low_disk",
        "disk.sccache",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    let schema = schema_field("disk");
    let properties = schema["properties"]
        .as_object()
        .expect("disk schema properties");
    for key in properties.keys() {
        let path = format!("disk.{key}");
        assert!(
            rendered.contains(&path) || omissions.contains(path.as_str()),
            "{path} must be rendered or dispositioned by an exact ledger row"
        );
    }
}

#[test]
fn hm_loc_and_replay_families_are_fully_rendered() {
    let rendered = rendered_paths(&module_src());
    for table in ["loc", "replay"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        let mut missing: Vec<String> = properties
            .keys()
            .filter_map(|key| {
                let path = format!("{table}.{key}");
                (!rendered.contains(&path)).then_some(path)
            })
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "Home Manager must render every {table} scalar; missing: {missing:?}"
        );
    }
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
        ("themePanePadding", "theme.pane_padding"),
        ("searchMaxResults", "search.max_results"),
        ("paletteContentMaxResults", "palette.content_max_results"),
        ("paletteFileMaxResults", "palette.file_max_results"),
        ("paletteSymbolMaxResults", "palette.symbol_max_results"),
        ("sessionScrollbackLines", "session.scrollback_lines"),
        ("sessionRestoreGraceSecs", "session.restore_grace_secs"),
        ("sessionResidentPoolLimit", "session.resident_pool_limit"),
        ("clipboardMaxImageBytes", "clipboard.max_image_bytes"),
        ("clipboardKeepHours", "clipboard.keep_hours"),
        ("locScanIntervalSecs", "loc.scan_interval_secs"),
        ("locMaxScanPerRound", "loc.max_scan_per_round"),
        ("locWatchInvalidateSecs", "loc.watch_invalidate_secs"),
        ("replayMaxBytesPerPane", "replay.max_bytes_per_pane"),
        ("replayMaxDurationSecs", "replay.max_duration_secs"),
        ("replayKeyframeIntervalMs", "replay.keyframe_interval_ms"),
        (
            "replayKeyframeIntervalBytes",
            "replay.keyframe_interval_bytes",
        ),
        ("replayIdleThresholdMs", "replay.idle_threshold_ms"),
        ("drawerPoolLimit", "drawer.pool_limit"),
        ("gitAutoFetchIntervalSecs", "git.auto_fetch_interval_secs"),
        (
            "gitAutoFetchMinIntervalSecs",
            "git.auto_fetch_min_interval_secs",
        ),
        ("recordingMaxBytes", "recording.max_bytes"),
        ("mediaSeekStepSecs", "media.seek_step_secs"),
        ("mediaSeekStepVideoSecs", "media.seek_step_video_secs"),
        ("mediaPollIntervalSecs", "media.poll_interval_secs"),
        ("monitorProcRows", "monitor.proc_rows"),
        ("limitsTestMaxParallel", "limits.test_max_parallel"),
        ("limitsTestTimeoutSecs", "limits.test_timeout_secs"),
        ("limitsDiscoverTimeoutSecs", "limits.discover_timeout_secs"),
        ("diagnosticsCrashRetention", "diagnostics.crash_retention"),
        ("diagnosticsRingSize", "diagnostics.ring_size"),
        ("diskWarnThresholdGb", "disk.warn_threshold_gb"),
        ("diskScanIntervalSecs", "disk.scan_interval_secs"),
        ("diskMaxScanPerRound", "disk.max_scan_per_round"),
        ("diskIdleCleanDays", "disk.idle_clean_days"),
        ("diskGenerationMinAgeDays", "disk.generation_min_age_days"),
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

        if schema.get("format").and_then(Value::as_str) == Some("uint16") {
            assert_eq!(
                nix_type, "lib.types.ints.u16",
                "Nix type for uint16 schema field {path}"
            );
            continue;
        }

        if schema.get("format").and_then(Value::as_str) == Some("uint32") {
            assert_eq!(
                nix_type, "lib.types.ints.u32",
                "Nix type for uint32 schema field {path}"
            );
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
        ("themeUndercurl", "theme.undercurl"),
        ("themeColor", "theme.color"),
        ("themeGlyphs", "theme.glyphs"),
        ("themeAgentGlyphs", "theme.agent_glyphs"),
        ("themeMascot", "theme.mascot"),
        ("themeMascotMotion", "theme.mascot_motion"),
        ("uiSidebarProjectSort", "ui.sidebar_project_sort"),
        ("uiSidebarTerminalsSection", "ui.sidebar_terminals_section"),
        ("uiSidebarFocusDetail", "ui.sidebar_focus_detail"),
        ("searchStructural", "search.structural"),
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
