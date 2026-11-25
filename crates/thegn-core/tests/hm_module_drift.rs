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
    ("editor", "editor integration is not exposed"),
    (
        "stats.net_icons",
        "per-medium network glyph table is not exposed",
    ),
    ("metrics", "metrics export is not exposed"),
    ("apps", "app ordering is not exposed"),
    ("observe", "Observe configuration is not exposed"),
    ("bars", "status bar configuration is not exposed"),
    ("calendar", "calendar sources are not exposed"),
    (
        "inbox",
        "inbox accounts run commands and carry credential refs; global config only, not exposed",
    ),
    ("weather.api_key", "credentials are not exposed"),
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
        "disk.sccache_dir",
        "path-valued shared-cache directory is not exposed",
    ),
    (
        "disk.shared_target_dir",
        "path-valued shared target directory is deliberately opt-in and is not exposed",
    ),
    ("notifications.rules", "routing rules are not exposed"),
    (
        "notifications.sound.command",
        "sound command is executable-bearing and is not exposed yet",
    ),
    (
        "notifications.sound.per_priority",
        "per-priority map holds legacy command-mode shell commands (executable-bearing) and is not exposed yet",
    ),
    ("notifications.modes", "notification modes are not exposed"),
    (
        "notifications.push",
        "push sinks carry credentials and are not exposed",
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
    (
        "lsp.servers",
        "language-server registry entries carry executables and are not exposed",
    ),
    (
        "daemon.socket",
        "path-valued control-socket override is not exposed",
    ),
    (
        "daemon.headless_producers",
        "headless producers run the owner's rules unattended; opt-in per machine, not exposed",
    ),
    ("serve", "remote serving policy is not exposed"),
    ("merge_queue", "merge queue policy is not exposed"),
    ("pr_queue", "PR queue policy is not exposed"),
    ("autopilot", "autopilot policy is not exposed"),
    ("jobs", "selected job runner is not exposed"),
    ("pipeline", "pipeline definitions are not exposed"),
    ("skills", "skill configuration is not exposed"),
    ("media.mpd.password", "credentials are not exposed"),
    ("voice", "voice input is not exposed"),
    ("lead", "Lead meta agent is experimental and not exposed"),
    (
        "usage.allow_network",
        "network opt-in awaits an owner decision",
    ),
    (
        "usage.accounts",
        "usage account credentials and paths are not exposed",
    ),
    ("model_proxy", "model proxy is not exposed"),
    ("remote", "remote connection tuning is not exposed"),
    (
        "host_discovery.kind",
        "host discovery backend enum needs destination pinning and is not exposed",
    ),
    (
        "host_discovery.tailnet.tag_filter",
        "tailnet tag filter is not exposed",
    ),
    (
        "host_discovery.tailnet.tailscale_bin",
        "tailnet executable override is not exposed",
    ),
    ("share", "port sharing is not exposed"),
    ("forward", "port forwarding is not exposed"),
    (
        "preview.ports",
        "candidate port list needs port-0 handling and is not exposed",
    ),
    (
        "preview.allow_external_urls",
        "SSRF boundary is a security opt-in and is not exposed as a convenience option",
    ),
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
        let is_submodule_container = matches!(name, "package" | "log" | "sandbox" | "manageConfig")
            || (name == "remote"
                && src[..declaration_offset].rfind("sandboxSubmodule =")
                    > src[..declaration_offset].rfind("remoteSubmodule ="));
        if !is_list_item_field && !is_submodule_container && name != "uiSidebarDetailFields" {
            offenders.push(format!("{name}: {value}"));
        }
    }
    assert!(
        offenders.is_empty(),
        "value options must default to null (allowed submodule containers and list-item fields excluded): {offenders:?}"
    );
}

#[test]
fn sidebar_detail_field_defaults_match_example_and_module() {
    let fields = [
        "branch",
        "commits",
        "dirty",
        "branch_stat",
        "pr",
        "ci",
        "issue",
        "author",
        "stash",
        "subject",
        "age",
        "upstream",
        "env",
        "sandbox",
        "devcontainer",
        "hibernated",
        "mq",
        "unread",
        "disk",
    ];
    let toml = format!(
        "sidebar_detail_fields = [{}]",
        fields
            .iter()
            .map(|field| format!("\"{field}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let nix = format!(
        "default = [{}];",
        fields
            .iter()
            .map(|field| format!("\"{field}\""))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let example = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config/config.toml.example"),
    )
    .expect("config/config.toml.example");
    assert!(
        example.contains(toml.as_str()),
        "config example default drifted"
    );
    assert!(
        module_src().contains(nix.as_str()),
        "Home Manager default drifted"
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

/// Nix enum options whose destination is a `String` validated by
/// `Priority::parse` (no schema enum exists for them).
const PRIORITY_ENUM_OPTIONS: &[&str] = &[
    "notificationsDndAllowPriority",
    "notificationsSoundMinPriority",
];

/// The priority spellings, from the Rust type (the single source of truth).
fn priority_spellings() -> Vec<&'static str> {
    use thegn_core::notification::Priority;
    [Priority::Info, Priority::Notice, Priority::Alert]
        .into_iter()
        .map(Priority::as_str)
        .collect()
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
        if !(expr.starts_with("lib.types.int")
            || expr.starts_with("lib.types.numbers.")
            || expr == "lib.types.port")
        {
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
        "daemon",
        "network",
        "preview",
        "usage",
        "lsp",
        "host_discovery",
        "host_discovery.tailnet",
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
        ("uiSidebarDetailWhen", "ui.sidebar_detail_when"),
        (
            "uiSidebarExternalWorktrees",
            "ui.sidebar_external_worktrees",
        ),
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
fn hm_git_enum_options_offer_only_canonical_spellings() {
    let expected = [
        ("gitBackend", "git.backend"),
        ("gitStructuralDiff", "git.structural_diff"),
        ("gitSubmodules", "git.submodules"),
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
fn hm_network_media_enum_options_offer_only_canonical_spellings() {
    let expected = [
        ("networkMode", "network.mode"),
        ("weatherUnits", "weather.units"),
        ("mediaDefaultAction", "media.default_action"),
        ("notificationsSoundMode", "notifications.sound.mode"),
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
fn hm_media_backend_offers_only_implemented_spellings() {
    let enums = enum_lists(&module_src());
    let values = enums
        .iter()
        .find(|(name, _)| name == "mediaBackend")
        .map(|(_, values)| values)
        .unwrap_or_else(|| panic!("Home Manager enum option `mediaBackend` is missing"));
    let actual: BTreeSet<&str> = values.iter().map(String::as_str).collect();
    let schema = schema_field("media.backend");
    let mut implemented: BTreeSet<&str> = schema["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("destination `media.backend` has no enum"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Reserved spellings (spotify, jellyfin) parse but have no backend, so HM
    // must not offer them.
    if let Some(marker) = schema.get(thegn_core::config_validate::ENUM_MARKER) {
        for reserved in marker["reserved"].as_array().into_iter().flatten() {
            if let Some(spelling) = reserved.as_str() {
                implemented.remove(spelling);
            }
        }
    }
    assert!(
        !implemented.is_empty(),
        "media.backend must retain implemented spellings"
    );
    assert_eq!(
        actual, implemented,
        "Home Manager mediaBackend must offer exactly the implemented spellings"
    );
}

#[test]
fn hm_weather_provider_offers_only_implemented_spellings() {
    let enums = enum_lists(&module_src());
    let values = enums
        .iter()
        .find(|(name, _)| name == "weatherProvider")
        .map(|(_, values)| values)
        .unwrap_or_else(|| panic!("Home Manager enum option `weatherProvider` is missing"));
    let actual: BTreeSet<&str> = values.iter().map(String::as_str).collect();
    let schema = schema_field("weather.provider");
    let mut implemented: BTreeSet<&str> = schema["enum"]
        .as_array()
        .unwrap_or_else(|| panic!("destination `weather.provider` has no enum"))
        .iter()
        .filter_map(Value::as_str)
        .collect();
    // Reserved spellings parse but have no provider, so HM must not offer them.
    if let Some(marker) = schema.get(thegn_core::config_validate::ENUM_MARKER) {
        for reserved in marker["reserved"].as_array().into_iter().flatten() {
            if let Some(spelling) = reserved.as_str() {
                implemented.remove(spelling);
            }
        }
    }
    assert!(
        !implemented.is_empty(),
        "weather.provider must retain implemented spellings"
    );
    assert_eq!(
        actual, implemented,
        "Home Manager weatherProvider must offer exactly the implemented spellings"
    );
}

#[test]
fn hm_sandbox_backend_offers_no_reserved_spelling() {
    let enums = enum_lists(&module_src());
    let values = enums
        .iter()
        .find(|(name, _)| name == "backend")
        .map(|(_, values)| values)
        .unwrap_or_else(|| panic!("Home Manager enum option `backend` is missing"));
    let schema = schema_field("sandbox.backend");
    let reserved: BTreeSet<&str> = schema
        .get(thegn_core::config_validate::ENUM_MARKER)
        .map(|marker| {
            marker["reserved"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect()
        })
        .unwrap_or_default();
    for spelling in ["smol", "wsl", "appcontainer"] {
        assert!(
            reserved.contains(spelling),
            "{spelling} must be reserved: {reserved:?}"
        );
    }
    assert!(
        values
            .iter()
            .all(|value| !reserved.contains(value.as_str())),
        "Home Manager `backend` offers a reserved spelling: {values:?}"
    );
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
                assert!(
                    rendered.contains(&path)
                        || (path == "stats.alerts" && path_is_rendered(&path, &rendered)),
                    "Home Manager must render {path}"
                );
            }
        }
    }
    for path in [
        "git.backend",
        "git.structural_diff",
        "git.submodules",
        "stats.net_icons",
    ] {
        assert!(schema_has_path(path), "omission must stay pinned: {path}");
    }

    for path in [
        "stats.refresh_secs",
        "stats.refresh_rates",
        "stats.battery_warn",
        "stats.disk_free_warn",
        "stats.disk_free_critical",
        "stats.alerts.enabled",
        "stats.alerts.cpu.warn",
    ] {
        assert!(
            path_is_rendered(path, &rendered),
            "Home Manager must render {path}"
        );
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
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }
    for path in [
        "notifications.priority",
        "notifications.active_mode",
        "notifications.agent_error_signatures",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }
    for path in [
        "notifications.rules",
        "notifications.sound.command",
        "notifications.sound.per_priority",
        "notifications.modes",
        "notifications.push",
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
        "notifications.sound.volume",
        "notifications.dnd.allow_priority",
        "notifications.dnd.windows",
        "notifications.sound.min_priority",
        "notifications.sound.per_kind",
        "notifications.sound.pack",
        "notifications.sound.chime_file",
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
                rendered.contains(&path)
                    || (path == "usage.alerts" && path_is_rendered(&path, &rendered))
                    || omissions.contains(path.as_str()),
                "{path} must be rendered or dispositioned by an exact ledger row"
            );
        }
    }
}

#[test]
fn hm_priority_enum_options_match_the_priority_type() {
    let enums = enum_lists(&module_src());
    for option in PRIORITY_ENUM_OPTIONS {
        let values = enums
            .iter()
            .find(|(name, _)| name == option)
            .map(|(_, values)| values.iter().map(String::as_str).collect::<Vec<&str>>())
            .unwrap_or_else(|| panic!("Home Manager enum option `{option}` is missing"));
        assert_eq!(values, priority_spellings(), "{option}");
    }
    let rendered = rendered_paths(&module_src());
    assert!(rendered.contains("notifications.sound.per_kind"));
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
        "media.volume_step",
        "media.mpv.socket",
        "media.mpd.socket",
        "monitor.proc_rows",
        "monitor.processes",
        "monitor.process_io",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["recording", "media", "media.mpv", "media.mpd", "monitor"] {
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            let path = format!("{table}.{key}");
            if matches!(path.as_str(), "media.mpv" | "media.mpd") {
                continue; // nested tables: checked leaf by leaf via their own entries above
            }
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
        "sandbox.limits.heavy_slots",
        "sandbox.limits.max_load",
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
fn hm_daemon_and_network_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "daemon.enabled",
        "daemon.idle_exit_secs",
        "daemon.lease_grace_secs",
        "daemon.stream_gap_window_secs",
        "network.offline_after_failures",
        "network.recovery_probe_secs",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for table in ["daemon", "network"] {
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
fn hm_preview_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "preview.enabled",
        "preview.fetch_timeout_ms",
        "preview.max_body_bytes",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    let schema = schema_field("preview");
    let properties = schema["properties"]
        .as_object()
        .expect("preview schema properties");
    for key in properties.keys() {
        let path = format!("preview.{key}");
        assert!(
            rendered.contains(&path) || omissions.contains(path.as_str()),
            "{path} must be rendered or dispositioned by an exact ledger row"
        );
    }
}

#[test]
fn hm_usage_and_tailnet_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "usage.enabled",
        "usage.statusbar",
        "usage.discover_profiles",
        "usage.token_rollups",
        "usage.history_days",
        "usage.poll_interval_secs",
        "usage.warn_percent",
        "usage.crit_percent",
        "usage.providers",
        "usage.profile_roots",
        "host_discovery.tailnet.enabled",
        "host_discovery.tailnet.online_only",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    let sections = ["usage", "host_discovery", "host_discovery.tailnet"];
    for section in sections {
        let schema = schema_field(section);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{section} schema properties"));
        for key in properties.keys() {
            let path = format!("{section}.{key}");
            if sections.contains(&path.as_str()) {
                continue;
            }
            assert!(
                path_is_rendered(&path, &rendered) || omissions.contains(path.as_str()),
                "{path} must be rendered or dispositioned by an exact ledger row"
            );
        }
    }
}

#[test]
fn hm_lsp_scalar_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in ["lsp.enabled", "lsp.hover", "lsp.require_containment"] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    let schema = schema_field("lsp");
    let properties = schema["properties"]
        .as_object()
        .expect("lsp schema properties");
    for key in properties.keys() {
        let path = format!("lsp.{key}");
        assert!(
            rendered.contains(&path) || omissions.contains(path.as_str()),
            "{path} must be rendered or dispositioned by an exact ledger row"
        );
    }
    assert!(omissions.contains("lsp.servers"));
}

#[test]
fn hm_semantic_scalar_keys_are_rendered_and_rest_dispositioned() {
    let rendered = rendered_paths(&module_src());
    for path in [
        "semantic.worktree_index",
        "semantic.index_max_files",
        "semantic.map_budget_lines",
    ] {
        assert!(rendered.contains(path), "Home Manager must render {path}");
    }

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    let schema = schema_field("semantic");
    let properties = schema["properties"]
        .as_object()
        .expect("semantic schema properties");
    for key in properties.keys() {
        let path = format!("semantic.{key}");
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
                || (!p.contains('.') && root_props.contains(p))
                // Nested maps of scalars (`notifications.sound.per_kind`) own
                // no documented keys either, so the walker never lists them;
                // accept any path the schema itself has.
                || schema_has_path(p))
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
        if PRIORITY_ENUM_OPTIONS.contains(&opt.as_str()) {
            // String-typed destinations validated by `Priority::parse`.
            let canonical = priority_spellings();
            assert!(
                vals.iter().all(|v| canonical.contains(&v.as_str())),
                "`{opt}` offers non-priority values: {vals:?}"
            );
            continue;
        }
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
        ("uiSidebarGroupRowLimit", "ui.sidebar_group_row_limit"),
        ("uiSidebarBatchGroupMin", "ui.sidebar_batch_group_min"),
        ("searchMaxResults", "search.max_results"),
        ("paletteContentMaxResults", "palette.content_max_results"),
        ("paletteFileMaxResults", "palette.file_max_results"),
        ("paletteSymbolMaxResults", "palette.symbol_max_results"),
        ("sessionScrollbackLines", "session.scrollback_lines"),
        ("sessionRestoreGraceSecs", "session.restore_grace_secs"),
        ("sessionResidentPoolLimit", "session.resident_pool_limit"),
        ("clipboardMaxImageBytes", "clipboard.max_image_bytes"),
        ("clipboardKeepHours", "clipboard.keep_hours"),
        ("clipboardPersistRegisters", "clipboard.persist_registers"),
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
        ("limitsHeavySlots", "sandbox.limits.heavy_slots"),
        ("limitsMaxLoad", "sandbox.limits.max_load"),
        ("mediaSeekStepSecs", "media.seek_step_secs"),
        ("mediaSeekStepVideoSecs", "media.seek_step_video_secs"),
        ("mediaPollIntervalSecs", "media.poll_interval_secs"),
        ("mediaVolumeStep", "media.volume_step"),
        ("notificationsSoundVolume", "notifications.sound.volume"),
        ("activityCpuPercent", "activity.cpu_percent"),
        (
            "activityRunawayCoreFraction",
            "activity.runaway_core_fraction",
        ),
        ("activityRunawaySecs", "activity.runaway_secs"),
        ("activityQuietGraceSecs", "activity.quiet_grace_secs"),
        ("activityResumeGraceSecs", "activity.resume_grace_secs"),
        ("activitySpawnGraceSecs", "activity.spawn_grace_secs"),
        (
            "activityUnsolicitedGapSecs",
            "activity.unsolicited_gap_secs",
        ),
        ("activityOutputHintTtlSecs", "activity.output_hint_ttl_secs"),
        ("monitorProcRows", "monitor.proc_rows"),
        ("limitsTestMaxParallel", "limits.test_max_parallel"),
        ("limitsTestNice", "limits.test_nice"),
        (
            "weatherRefreshIntervalSecs",
            "weather.refresh_interval_secs",
        ),
        ("weatherStaleAfterSecs", "weather.stale_after_secs"),
        ("weatherHardExpirySecs", "weather.hard_expiry_secs"),
        ("weatherForecastDays", "weather.forecast_days"),
        ("weatherTimeoutSecs", "weather.timeout_secs"),
        ("limitsTestTimeoutSecs", "limits.test_timeout_secs"),
        ("limitsDiscoverTimeoutSecs", "limits.discover_timeout_secs"),
        ("diagnosticsCrashRetention", "diagnostics.crash_retention"),
        ("diagnosticsRingSize", "diagnostics.ring_size"),
        ("diskWarnThresholdGb", "disk.warn_threshold_gb"),
        ("diskScanIntervalSecs", "disk.scan_interval_secs"),
        ("diskMaxScanPerRound", "disk.max_scan_per_round"),
        ("diskIdleCleanDays", "disk.idle_clean_days"),
        ("diskGenerationMinAgeDays", "disk.generation_min_age_days"),
        ("daemonIdleExitSecs", "daemon.idle_exit_secs"),
        ("daemonLeaseGraceSecs", "daemon.lease_grace_secs"),
        ("daemonStreamGapWindowSecs", "daemon.stream_gap_window_secs"),
        (
            "networkOfflineAfterFailures",
            "network.offline_after_failures",
        ),
        ("networkRecoveryProbeSecs", "network.recovery_probe_secs"),
        ("previewFetchTimeoutMs", "preview.fetch_timeout_ms"),
        ("previewMaxBodyBytes", "preview.max_body_bytes"),
        ("usageHistoryDays", "usage.history_days"),
        ("usagePollIntervalSecs", "usage.poll_interval_secs"),
        ("usageWarnPercent", "usage.warn_percent"),
        ("usageCritPercent", "usage.crit_percent"),
        ("semanticIndexMaxFiles", "semantic.index_max_files"),
        ("semanticMapBudgetLines", "semantic.map_budget_lines"),
        ("statsRefreshSecs", "stats.refresh_secs"),
        ("statsBatteryWarn", "stats.battery_warn"),
        ("statsDiskFreeWarn", "stats.disk_free_warn"),
        ("statsDiskFreeCritical", "stats.disk_free_critical"),
        ("stderrCapMb", "log.stderr_cap_mb"),
    ];
    let options = numeric_options(&module_src());
    let actual_names: BTreeSet<&str> = options.iter().map(|(name, _)| name.as_str()).collect();
    let expected_names: BTreeSet<&str> = expected.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        actual_names, expected_names,
        "every Nix integer/port type must have an explicit Rust schema destination"
    );

    let mut pinned_floats = BTreeSet::new();
    for (name, nix_type) in options {
        let path = expected
            .iter()
            .find_map(|(option, path)| (*option == name).then_some(*path))
            .expect("numeric option destination");
        let schema = schema_field(path);
        let schema_type = schema.get("type").and_then(Value::as_str);
        if schema_type == Some("number") {
            let format = schema.get("format").and_then(Value::as_str);
            if let Some((min, max)) = float_range(path) {
                assert!(
                    matches!(format, None | Some("double" | "float")),
                    "schema number format for {path} must be f64 or f32, got {format:?}"
                );
                if let Some(maximum) = schema.get("maximum").and_then(Value::as_f64) {
                    assert_eq!(maximum, max, "schema maximum for pinned float field {path}");
                }
                assert_eq!(
                    nix_type,
                    format!("lib.types.numbers.between {min} {max}"),
                    "Nix type for pinned float field {path}"
                );
                pinned_floats.insert(path);
                continue;
            }
            assert!(
                format.is_none() || format == Some("double"),
                "schema number format for {path} must be absent, got {format:?}"
            );
            let minimum = schema.get("minimum").and_then(Value::as_f64);
            let maximum = schema.get("maximum").and_then(Value::as_f64);
            match (minimum, maximum) {
                (Some(0.0), None) => {
                    assert_eq!(
                        nix_type, "lib.types.numbers.nonnegative",
                        "Nix type for non-negative float field {path}"
                    );
                }
                (Some(minimum), Some(maximum)) => {
                    assert_eq!(
                        nix_type,
                        format!("lib.types.numbers.between {minimum} {maximum}"),
                        "Nix type for bounded float field {path}"
                    );
                }
                _ => panic!("float schema range for {path} is not pinned; add a Nix mapping"),
            }
            continue;
        }
        assert_eq!(schema_type, Some("integer"), "schema type for {path}");
        if path == "preview.fetch_timeout_ms" {
            assert_eq!(
                nix_type,
                format!(
                    "lib.types.ints.between {} {}",
                    thegn_core::config_preview::MIN_FETCH_TIMEOUT_MS,
                    thegn_core::config_preview::MAX_FETCH_TIMEOUT_MS
                ),
                "Nix type for clamped field {path}"
            );
            continue;
        }
        if path == "preview.max_body_bytes" {
            assert_eq!(
                nix_type,
                format!(
                    "lib.types.ints.between 1 {}",
                    thegn_core::config_preview::MAX_PREVIEW_BODY_BYTES
                ),
                "Nix type for clamped field {path}"
            );
            continue;
        }
        if path == "usage.history_days" {
            assert_eq!(
                nix_type,
                format!(
                    "lib.types.ints.between 0 {}",
                    thegn_core::time_policy::MAX_DURATION_DAYS
                ),
                "Nix type for clamped field {path}"
            );
            continue;
        }
        if path == "usage.poll_interval_secs" {
            assert_eq!(
                nix_type,
                format!(
                    "lib.types.ints.between 60 {}",
                    thegn_core::time_policy::MAX_CADENCE_SECS
                ),
                "Nix type for clamped field {path}"
            );
            continue;
        }
        if matches!(
            path,
            "semantic.index_max_files" | "semantic.map_budget_lines"
        ) {
            assert_eq!(
                nix_type, "lib.types.ints.positive",
                "Nix type for floored field {path}"
            );
            continue;
        }
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

        if schema.get("format").and_then(Value::as_str) == Some("int32") {
            assert!(
                signed_nix_type_fits_i32(&nix_type),
                "Nix type for int32 schema field {path} must be ints.s32 or a between within i32, got {nix_type}"
            );
            continue;
        }

        if schema.get("format").and_then(Value::as_str) == Some("uint8") {
            assert_eq!(
                nix_type, "lib.types.ints.u8",
                "Nix type for uint8 schema field {path}"
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
    assert_eq!(
        pinned_floats,
        FLOAT_RANGES
            .iter()
            .map(|(path, _, _)| *path)
            .collect::<BTreeSet<&str>>(),
        "every pinned float range must map to a rendered Nix option"
    );
}

/// Whether a Nix integer type expression is a signed type confined to i32:
/// `lib.types.ints.s32` or `lib.types.ints.between <lo> <hi>` within i32.
fn signed_nix_type_fits_i32(nix_type: &str) -> bool {
    if nix_type == "lib.types.ints.s32" {
        return true;
    }
    let Some(rest) = nix_type.strip_prefix("lib.types.ints.between ") else {
        return false;
    };
    let bounds: Vec<&str> = rest
        .split_whitespace()
        .map(|b| b.trim_matches(|c| c == '(' || c == ')'))
        .collect();
    let [lo, hi] = bounds[..] else {
        return false;
    };
    matches!((lo.parse::<i32>(), hi.parse::<i32>()), (Ok(l), Ok(h)) if l <= h)
}

#[test]
fn signed_range_gate_accepts_i32_types_and_rejects_others() {
    assert!(signed_nix_type_fits_i32("lib.types.ints.s32"));
    assert!(signed_nix_type_fits_i32("lib.types.ints.between (-20) 19"));
    assert!(!signed_nix_type_fits_i32("lib.types.ints.unsigned"));
    assert!(!signed_nix_type_fits_i32("lib.types.ints.u32"));
    assert!(!signed_nix_type_fits_i32(
        "lib.types.ints.between (-3000000000) 19"
    ));
    // A u64 destination must not be satisfied by the signed type.
    let schema = schema_field("limits.test_timeout_secs");
    assert_ne!(schema.get("format").and_then(Value::as_str), Some("int32"));
}

#[test]
fn hm_weather_and_test_nice_are_rendered_with_api_key_omitted() {
    let rendered = rendered_paths(&module_src());
    for key in [
        "enabled",
        "provider",
        "location",
        "units",
        "refresh_interval_secs",
        "stale_after_secs",
        "hard_expiry_secs",
        "show_forecast",
        "forecast_days",
        "timeout_secs",
    ] {
        let path = format!("weather.{key}");
        assert!(rendered.contains(path.as_str()), "must render {path}");
    }
    assert!(rendered.contains("limits.test_nice"));
    assert!(!rendered.contains("weather.api_key"));
    let schema = schema_field("weather");
    for key in schema["properties"]
        .as_object()
        .expect("weather props")
        .keys()
    {
        let path = format!("weather.{key}");
        assert!(
            rendered.contains(path.as_str()) || OMITTED.iter().any(|(p, _)| *p == path),
            "{path} must be rendered or omitted"
        );
    }
    assert_eq!(
        schema_field("limits.test_nice")
            .get("format")
            .and_then(Value::as_str),
        Some("int32")
    );
}

/// Float keys whose valid range is enforced outside the schema (a Rust
/// validator, or a schema maximum whose minimum comes from the duration walk).
/// Each range is pinned three ways: the Nix `numbers.between` type, the Rust
/// validator's endpoints, and `config_validate::FLOAT_KEY_RANGES`.
const FLOAT_RANGES: &[(&str, f64, f64)] = &[
    (
        "stats.refresh_secs",
        0.0,
        thegn_core::time_policy::MAX_CADENCE_SECS as f64,
    ),
    ("media.volume_step", 0.0, 1.0),
    // Validated as an ERROR by `SoundConfig::validate_at`, not warn-only.
    ("notifications.sound.volume", 0.0, 1.0),
    ("usage.warn_percent", 0.0, 100.0),
    ("usage.crit_percent", 0.0, 100.0),
    ("activity.cpu_percent", 0.0, 100.0),
    ("activity.runaway_core_fraction", 0.0, 1.0),
    (
        "activity.runaway_secs",
        0.0,
        thegn_core::time_policy::MAX_DURATION_SECS as f64,
    ),
    (
        "activity.quiet_grace_secs",
        0.0,
        thegn_core::time_policy::MAX_DURATION_SECS as f64,
    ),
    (
        "activity.resume_grace_secs",
        0.0,
        thegn_core::time_policy::MAX_DURATION_SECS as f64,
    ),
    (
        "activity.spawn_grace_secs",
        0.0,
        thegn_core::time_policy::MAX_DURATION_SECS as f64,
    ),
    (
        "activity.unsolicited_gap_secs",
        0.0,
        thegn_core::time_policy::MAX_DURATION_SECS as f64,
    ),
    (
        "activity.output_hint_ttl_secs",
        0.0,
        thegn_core::time_policy::MAX_DURATION_SECS as f64,
    ),
];

fn float_range(path: &str) -> Option<(f64, f64)> {
    FLOAT_RANGES
        .iter()
        .find_map(|(candidate, min, max)| (*candidate == path).then_some((*min, *max)))
}

/// `(errors, warnings)` naming `path` for a one-key config body.
fn float_key_diagnostics(path: &str, value: &str) -> (Vec<String>, Vec<String>) {
    use thegn_core::config_validate::{ValidationSeverity, validate_diagnostics};
    let (section, key) = path.split_once('.').expect("dotted float key");
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for diagnostic in validate_diagnostics(&format!("[{section}]\n{key} = {value}\n")) {
        if !diagnostic.message.contains(path) {
            continue;
        }
        match diagnostic.severity {
            ValidationSeverity::Error => errors.push(diagnostic.message),
            ValidationSeverity::Warning => warnings.push(diagnostic.message),
        }
    }
    (errors, warnings)
}

/// The Rust-validated float display keys WARN (never refuse startup) on an
/// out-of-range or non-finite value; the schema-ranged `stats.refresh_secs`
/// keeps its existing error. Endpoints produce nothing either way.
#[test]
fn pinned_float_ranges_match_the_rust_validators() {
    let warn_only: BTreeSet<&str> = thegn_core::config_validate::FLOAT_KEY_RANGES
        .iter()
        .map(|(path, _, _)| *path)
        .collect();
    for (path, min, max) in thegn_core::config_validate::FLOAT_KEY_RANGES {
        assert_eq!(
            float_range(path),
            Some((min, max)),
            "validator range for {path} must be pinned in FLOAT_RANGES"
        );
    }
    for (path, min, max) in FLOAT_RANGES {
        for accepted in [*min, *max] {
            let (errors, warnings) = float_key_diagnostics(path, &format!("{accepted:?}"));
            assert!(errors.is_empty(), "{path} = {accepted}: {errors:?}");
            assert!(warnings.is_empty(), "{path} = {accepted}: {warnings:?}");
        }
        for out_of_range in [
            format!("{:?}", min - 0.001),
            format!("{:?}", max + 0.001),
            "nan".to_string(),
            "inf".to_string(),
        ] {
            let (errors, warnings) = float_key_diagnostics(path, &out_of_range);
            if warn_only.contains(path) {
                // TOML `nan`/`inf` have no JSON number, so the schema walk's
                // pre-existing type check still names them ("expected number,
                // got null"); the range validator itself never errors.
                let finite = out_of_range != "nan" && out_of_range != "inf";
                assert!(
                    errors
                        .iter()
                        .all(|error| { !finite && error.contains("expected number, got null") }),
                    "{path} = {out_of_range} must not refuse on range: {errors:?}"
                );
                assert_eq!(
                    warnings.len(),
                    1,
                    "{path} = {out_of_range} must warn: {warnings:?}"
                );
                assert!(
                    warnings[0].contains(&format!("{min}..={max}")),
                    "{warnings:?}"
                );
            } else {
                assert!(
                    !errors.is_empty(),
                    "{path} = {out_of_range} must be rejected"
                );
            }
        }
    }
}

/// Options whose Nix type is a list of bounded floats, with their Rust schema
/// destination and the warn-only validator range each element is pinned to.
const FLOAT_LIST_OPTIONS: &[(&str, &str, (f64, f64))] = &[(
    "statsRefreshRates",
    "stats.refresh_rates",
    thegn_core::config_validate::REFRESH_RATE_RANGE,
)];

/// `(option name, element type)` for every `listOf (lib.types.numbers.…)`
/// option in the module.
fn float_list_options(src: &str) -> Vec<(String, String)> {
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
        let Some(element) = strip_null_or(expr)
            .strip_prefix("lib.types.listOf (")
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        if !element.starts_with("lib.types.numbers.") {
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
            .unwrap_or_else(|| format!("float-list@{}", i + 1));
        out.push((name, element.to_string()));
    }
    out
}

#[test]
fn every_float_list_option_matches_its_validator_range() {
    let options = float_list_options(&module_src());
    assert_eq!(
        options
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<BTreeSet<&str>>(),
        FLOAT_LIST_OPTIONS
            .iter()
            .map(|(name, _, _)| *name)
            .collect::<BTreeSet<&str>>(),
        "every Nix float-list option must have a pinned destination and range"
    );
    let rendered = rendered_paths(&module_src());
    for (name, path, (min, max)) in FLOAT_LIST_OPTIONS {
        assert!(rendered.contains(*path), "Home Manager must render {path}");
        let schema = schema_field(path);
        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("array"),
            "schema type for {path}"
        );
        assert_eq!(
            schema["items"].get("type").and_then(Value::as_str),
            Some("number"),
            "schema element type for {path}"
        );
        let element = options
            .iter()
            .find_map(|(option, element)| (option == name).then_some(element.as_str()))
            .expect("float-list option element type");
        assert_eq!(
            element,
            format!("lib.types.numbers.between {min} {max}"),
            "Nix element type for {path}"
        );
    }
    assert_eq!(
        thegn_core::config_validate::REFRESH_RATE_RANGE,
        (0.5, thegn_core::time_policy::MAX_DURATION_SECS as f64)
    );
}

/// Each `stats.refresh_rates` entry warns (never errors) outside the pinned
/// range, once per offending entry; the endpoints and an empty list are clean.
#[test]
fn refresh_rates_entries_warn_but_never_refuse() {
    let (min, max) = thegn_core::config_validate::REFRESH_RATE_RANGE;
    for accepted in [format!("[{min:?}, {max:?}]"), "[]".to_string()] {
        let (errors, warnings) = float_key_diagnostics("stats.refresh_rates", &accepted);
        assert!(errors.is_empty(), "{accepted}: {errors:?}");
        assert!(warnings.is_empty(), "{accepted}: {warnings:?}");
    }
    let body = format!("[1.0, {:?}, 2.0, {:?}]", min - 0.001, max + 0.001);
    let (errors, warnings) = float_key_diagnostics("stats.refresh_rates", &body);
    assert!(errors.is_empty(), "{body}: {errors:?}");
    assert_eq!(warnings.len(), 2, "{body}: {warnings:?}");
    assert!(warnings[0].starts_with("stats.refresh_rates[1] = "));
    assert!(warnings[1].starts_with("stats.refresh_rates[3] = "));
    for warning in &warnings {
        assert!(warning.contains(&format!("{min}..={max}")), "{warning}");
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
        ("uiSidebarDetailWhen", "ui.sidebar_detail_when"),
        (
            "uiSidebarExternalWorktrees",
            "ui.sidebar_external_worktrees",
        ),
        ("searchStructural", "search.structural"),
        ("gitBackend", "git.backend"),
        ("gitStructuralDiff", "git.structural_diff"),
        ("gitSubmodules", "git.submodules"),
        ("networkMode", "network.mode"),
        ("mediaDefaultAction", "media.default_action"),
        ("mediaBackend", "media.backend"),
        ("notificationsSoundMode", "notifications.sound.mode"),
        (
            "notificationsDndAllowPriority",
            "notifications.dnd.allow_priority",
        ),
        (
            "notificationsSoundMinPriority",
            "notifications.sound.min_priority",
        ),
        ("weatherProvider", "weather.provider"),
        ("weatherUnits", "weather.units"),
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
        if PRIORITY_ENUM_OPTIONS.contains(&name.as_str()) {
            // The destination is a plain `String` validated by `Priority::parse`,
            // so the schema carries no enum; compare against the Rust type.
            let values: Vec<&str> = values.iter().map(String::as_str).collect();
            assert_eq!(
                values,
                priority_spellings(),
                "`{name}` must offer `{path}` priorities"
            );
            continue;
        }
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

/// Alert tables are rendered as one submodule attrset per table. Their leaf
/// names are the TOML keys, so pin the whole set against the Rust schema, and
/// pin that executable-bearing sound keys stay unexposed.
#[test]
fn hm_alert_submodules_cover_every_schema_leaf_and_default_to_null() {
    let src = module_src();
    let rendered = rendered_paths(&src);
    for path in ["stats.alerts", "usage.alerts"] {
        assert!(
            path_is_rendered(path, &rendered),
            "Home Manager must render {path}"
        );
    }
    for (submodule, table) in [
        ("statsAlertsSubmodule", "stats.alerts"),
        ("usageAlertsSubmodule", "usage.alerts"),
    ] {
        let start = src
            .find(&format!("  {submodule} ="))
            .unwrap_or_else(|| panic!("{submodule} declaration"));
        let end = start
            + src[start..]
                .find("\n  };\n")
                .unwrap_or_else(|| panic!("{submodule} end"));
        let block = &src[start..end];
        let schema = schema_field(table);
        let properties = schema["properties"]
            .as_object()
            .unwrap_or_else(|| panic!("{table} schema properties"));
        for key in properties.keys() {
            assert!(
                block.contains(&format!("      {key} = lib.mkOption {{")),
                "{submodule} must expose {table}.{key}"
            );
        }
        let declared = block.matches("= lib.mkOption {").count();
        assert_eq!(declared, properties.len(), "{submodule} option count");
        assert_eq!(
            block.matches("default = null;").count(),
            declared,
            "every {submodule} leaf must default to null"
        );
    }
    let rule = schema_field("stats.alerts.cpu");
    let rule_keys: BTreeSet<&str> = rule["properties"]
        .as_object()
        .expect("AlertRule properties")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(rule_keys, BTreeSet::from(["warn", "critical"]));
    let rule_start = src
        .find("  alertRuleSubmodule =")
        .expect("alertRuleSubmodule declaration");
    let rule_block = &src[rule_start..rule_start + 600];
    assert!(rule_block.contains("      warn = lib.mkOption {"));
    assert!(rule_block.contains("      critical = lib.mkOption {"));

    let omissions: BTreeSet<&str> = OMITTED.iter().map(|(path, _)| *path).collect();
    for path in [
        "notifications.sound.command",
        "notifications.sound.per_priority",
    ] {
        assert!(omissions.contains(path), "{path} must stay omitted");
        assert!(!rendered.contains(path), "{path} must not be rendered");
    }
    assert!(src.contains("alertSecs = lib.types.nullOr lib.types.ints.u32;"));
}
