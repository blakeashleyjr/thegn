//! Strict validation for `thegn config validate` / `config set`.
//!
//! Schema-driven: `config_enum!` emits a manual `JsonSchema` impl carrying the
//! canonical values + aliases under the [`ENUM_MARKER`] extension, and
//! [`validate_str`] walks the raw TOML document in lockstep with
//! `schema_for!(Config)`, strict-checking every marked string node. Because
//! the schema is generated from the exact serde structure that loads the file,
//! every enum reachable from [`Config`]'s type tree is covered by
//! construction — new enums and new fields need no registration here.
//!
//! Honest coverage contract: "every `config_enum!` reachable from `Config`'s
//! serde tree is validated". `ShareReach` is intentionally out of scope (it
//! has no config.toml key — CLI/runtime vocabulary only), and the flattened
//! `[keybinds]` map is skipped (schemars 0.8 drops a flattened map's
//! `additionalProperties`, so its free-form keys are simply not traversed).

use std::sync::OnceLock;

use schemars::schema::{RootSchema, Schema, SchemaObject, SingleOrVec};

use crate::config::Config;

/// Schema-extension key `config_enum!` stamps on every enum it defines; the
/// value is `{ "kind": <human kind>, "aliases": [<accepted aliases>] }`.
pub const ENUM_MARKER: &str = "x-thegn-enum";

/// The `Config` schema, generated once (it is pure and deterministic).
fn config_schema() -> &'static RootSchema {
    static SCHEMA: OnceLock<RootSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| schemars::schema_for!(Config))
}

/// Strictly validate a raw `config.toml` body, collecting human-readable errors
/// for `config validate` (the only place a bad value is treated as an error
/// rather than warned-and-defaulted). Returns the list of problems (empty = ok).
pub fn validate_str(body: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let val: toml::Value = match body.parse() {
        Ok(v) => v,
        Err(e) => return vec![format!("TOML syntax error: {e}")],
    };
    // The wholesale check load_layered enforces: if the body fails to
    // deserialize into `Config`, the entire file is discarded for defaults.
    // This catches shape/type errors; the schema walk below catches the
    // warn-and-default enum values `Deserialize` never rejects.
    match toml::from_str::<Config>(body) {
        Err(e) => errs.push(format!("config would be rejected on load: {e}")),
        // Templates are strings as far as the schema is concerned, so their
        // placeholders can only be checked once the file has deserialized.
        Ok(cfg) => {
            check_templates(&cfg, &mut errs);
            // IANA zone names can't be a `config_enum!` (~600 of them, and the
            // list rots with each tzdb release), so `[calendar]` is checked
            // against the bundled database here instead — with a did-you-mean.
            errs.extend(crate::config_calendar::validate_calendar(&cfg.calendar));
            // The push command inbox: enabling it demands a SecretRef secret,
            // a non-empty allow list of known non-admin capabilities, and valid
            // scopes — a subscribed-but-inert inbox is not a valid state.
            errs.extend(cfg.notifications.push.inbox.validate_errors());
            // The crash-forwarding sink is a reserved provider-seam kind — a
            // non-empty value is rejected (not silently ignored).
            if let Err(e) = cfg.diagnostics.validate_crash_sink() {
                errs.push(e);
            }
        }
    }
    let root = config_schema();
    walk_object(&root.schema, root, &val, "", &mut errs);
    errs
}

/// Check every agent prompt/command template against the variables its surface
/// actually provides. A typo like `{branchh}` would otherwise reach the agent as
/// an empty expansion mid-drain; here it is a `config validate` error.
fn check_templates(cfg: &Config, errs: &mut Vec<String>) {
    use crate::agent_task::{COMMAND_VARS, TaskKind, validate_template};

    let mut check = |key: &str, template: &str, allowed: &[&str], is_command: bool| {
        if template.trim().is_empty() {
            return;
        }
        if let Err(e) = validate_template(template, allowed, is_command) {
            errs.push(format!("{key}: {e}"));
        }
    };

    let mq = &cfg.merge_queue;
    check(
        "merge_queue.agent_command",
        &mq.agent_command,
        COMMAND_VARS,
        true,
    );
    check(
        "merge_queue.prompts.conflict",
        &mq.prompts.conflict,
        TaskKind::MergeConflict.prompt_vars(),
        false,
    );
    check(
        "merge_queue.prompts.gate_failure",
        &mq.prompts.gate_failure,
        TaskKind::GateFailure.prompt_vars(),
        false,
    );

    let pq = &cfg.pr_queue;
    check(
        "pr_queue.agent_command",
        &pq.agent_command,
        COMMAND_VARS,
        true,
    );
    for (key, template, kind) in [
        (
            "pr_queue.prompts.ci_failure",
            &pq.prompts.ci_failure,
            TaskKind::PrCiFailure,
        ),
        (
            "pr_queue.prompts.conflict",
            &pq.prompts.conflict,
            TaskKind::PrConflict,
        ),
        (
            "pr_queue.prompts.review",
            &pq.prompts.review,
            TaskKind::PrReview,
        ),
    ] {
        check(key, template, kind.prompt_vars(), false);
    }

    // The per-repo layer carries the same keys, so it needs the same check —
    // otherwise a bad template hides in a `[workspace.<slug>]` block until that
    // repo happens to drain.
    for (slug, ws) in &cfg.workspace {
        let p = &ws.pr_queue;
        if let Some(c) = p.agent_command.as_deref() {
            check(
                &format!("workspace.{slug}.pr_queue.agent_command"),
                c,
                COMMAND_VARS,
                true,
            );
        }
        for (name, template, kind) in [
            ("ci_failure", &p.prompts.ci_failure, TaskKind::PrCiFailure),
            ("conflict", &p.prompts.conflict, TaskKind::PrConflict),
            ("review", &p.prompts.review, TaskKind::PrReview),
        ] {
            if let Some(t) = template.as_deref() {
                check(
                    &format!("workspace.{slug}.pr_queue.prompts.{name}"),
                    t,
                    kind.prompt_vars(),
                    false,
                );
            }
        }

        let o = &ws.merge_queue;
        if let Some(c) = o.agent_command.as_deref() {
            check(
                &format!("workspace.{slug}.merge_queue.agent_command"),
                c,
                COMMAND_VARS,
                true,
            );
        }
        if let Some(t) = o.prompts.conflict.as_deref() {
            check(
                &format!("workspace.{slug}.merge_queue.prompts.conflict"),
                t,
                TaskKind::MergeConflict.prompt_vars(),
                false,
            );
        }
        if let Some(t) = o.prompts.gate_failure.as_deref() {
            check(
                &format!("workspace.{slug}.merge_queue.prompts.gate_failure"),
                t,
                TaskKind::GateFailure.prompt_vars(),
                false,
            );
        }
    }
}

/// Resolve a `$ref` (`#/definitions/Name`) to its definition name.
fn ref_name(reference: &str) -> &str {
    reference.rsplit('/').next().unwrap_or(reference)
}

fn walk_schema(
    schema: &Schema,
    root: &RootSchema,
    value: &toml::Value,
    path: &str,
    errs: &mut Vec<String>,
) {
    // `Schema::Bool` (e.g. `additionalProperties: false`) constrains shape,
    // which the wholesale `Config` parse already enforces — nothing enum-shaped
    // to check.
    if let Schema::Object(obj) = schema {
        walk_object(obj, root, value, path, errs);
    }
}

/// Walk one schema node in lockstep with the TOML value it describes.
fn walk_object(
    obj: &SchemaObject,
    root: &RootSchema,
    value: &toml::Value,
    path: &str,
    errs: &mut Vec<String>,
) {
    // Resolve `$ref` through the root's definitions.
    if let Some(reference) = &obj.reference {
        if let Some(def) = root.definitions.get(ref_name(reference)) {
            walk_schema(def, root, value, path, errs);
        }
        return;
    }
    // schemars 0.8 wraps a `$ref` field in `allOf` whenever the field carries
    // ANY metadata (a `default`, a doc comment, …), and `Option<Enum>` becomes
    // `anyOf [$ref, null]` — recurse all subschema lists unconditionally. Only
    // the resolved enum definition carries [`ENUM_MARKER`], so a value reached
    // through a wrapper is still checked exactly once (never double-reported).
    if let Some(sub) = &obj.subschemas {
        for list in [&sub.all_of, &sub.any_of, &sub.one_of]
            .into_iter()
            .flatten()
        {
            for s in list {
                walk_schema(s, root, value, path, errs);
            }
        }
    }
    // The strict enum check: only nodes carrying the `config_enum!` marker,
    // and only string TOML values — `failover` keys legally accept a bool
    // (`de_failover`), and genuinely wrong types are already reported by the
    // wholesale `Config` parse above.
    if let toml::Value::String(s) = value
        && let Some(marker) = obj.extensions.get(ENUM_MARKER)
        && let Err(e) = check_enum(obj, marker, s)
    {
        errs.push(format!("{path}: {e}"));
    }
    match value {
        toml::Value::Table(table) => {
            if let Some(ov) = &obj.object {
                for (key, child) in table {
                    let child_path = join_key(path, key);
                    if let Some(prop) = ov.properties.get(key) {
                        walk_schema(prop, root, child, &child_path, errs);
                    } else if let Some(additional) = &ov.additional_properties {
                        // Map tables: `[env.<name>]`, `[host.<name>]`, …
                        walk_schema(additional, root, child, &child_path, errs);
                    } else if !LEGACY_KEYS.contains(&child_path.as_str()) {
                        // Not in the schema: the lenient loader drops it
                        // silently (a typo'd key is the classic "my config
                        // does nothing" bug), so strict validation names it,
                        // with a nearest-key hint when one is close.
                        let hint = nearest_key(key, ov.properties.keys().map(String::as_str));
                        errs.push(match hint {
                            Some(h) => format!("{child_path}: unknown key (did you mean `{h}`?)"),
                            None => format!("{child_path}: unknown key"),
                        });
                    }
                }
            }
        }
        toml::Value::Array(items) => {
            if let Some(av) = &obj.array
                && let Some(SingleOrVec::Single(item)) = &av.items
            {
                for (i, child) in items.iter().enumerate() {
                    walk_schema(item, root, child, &format!("{path}[{i}]"), errs);
                }
            }
        }
        _ => {}
    }
}

/// Mirror of `from_str_validated`: trim + ASCII-lowercase, match canonical
/// values then aliases, and reproduce its exact error message on a miss.
fn check_enum(obj: &SchemaObject, marker: &serde_json::Value, raw: &str) -> Result<(), String> {
    let norm = raw.trim().to_ascii_lowercase();
    // Reserved spellings are accepted by the lenient loader (warn + default)
    // but are a strict-validation error: the value names a provider this
    // build does not implement, and silently defaulting is not what the user
    // asked for.
    if marker
        .get("reserved")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .any(|r| r == norm)
    {
        let kind = marker
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("value");
        return Err(format!(
            "{kind} {norm:?} is reserved: accepted by config but not implemented in this build"
        ));
    }
    let canon: Vec<&str> = obj
        .enum_values
        .iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .collect();
    if canon.iter().any(|c| *c == norm) {
        return Ok(());
    }
    if marker
        .get("aliases")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .any(|a| a == norm)
    {
        return Ok(());
    }
    let kind = marker
        .get("kind")
        .and_then(|k| k.as_str())
        .unwrap_or("value");
    Err(format!(
        "unknown {kind} {norm:?}; expected one of: {}",
        canon.join(", ")
    ))
}

/// Top-level keys that were once real and are now explicitly tolerated by the
/// lenient loader with their own warning (`Config::load` names them); strict
/// validation stays quiet about them so the two messages don't double up.
const LEGACY_KEYS: &[&str] = &["llm_proxy"];

/// The closest known key by edit distance, when it is close enough to be a
/// plausible typo (≤ 2 edits, or a case/underscore/hyphen variant).
fn nearest_key<'a>(unknown: &str, known: impl Iterator<Item = &'a str>) -> Option<&'a str> {
    let norm = |s: &str| s.to_ascii_lowercase().replace('-', "_");
    let u = norm(unknown);
    let mut best: Option<(usize, &str)> = None;
    for k in known {
        let d = edit_distance(&u, &norm(k));
        if d <= 2 && best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, k));
        }
    }
    best.map(|(_, k)| k)
}

fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Dotted key join; arrays use bracket form (`pins[0].location`).
fn join_key(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_string()
    } else {
        format!("{path}.{key}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structural walk of the schema (no TOML document): collect every
    /// `(path, enum name)` pair reachable from the root. Maps contribute a `*`
    /// path segment, arrays `[]`. Definition recursion is cycle-guarded.
    fn collect_enum_paths(
        schema: &Schema,
        root: &RootSchema,
        path: &str,
        stack: &mut Vec<String>,
        out: &mut Vec<(String, String)>,
    ) {
        let Schema::Object(obj) = schema else { return };
        if let Some(reference) = &obj.reference {
            let name = ref_name(reference).to_string();
            if stack.contains(&name) {
                return; // cycle guard
            }
            if let Some(def) = root.definitions.get(&name) {
                if let Schema::Object(dobj) = def
                    && dobj.extensions.contains_key(ENUM_MARKER)
                {
                    out.push((path.to_string(), name.clone()));
                }
                stack.push(name);
                collect_enum_paths(def, root, path, stack, out);
                stack.pop();
            }
            return;
        }
        if let Some(sub) = &obj.subschemas {
            for list in [&sub.all_of, &sub.any_of, &sub.one_of]
                .into_iter()
                .flatten()
            {
                for s in list {
                    collect_enum_paths(s, root, path, stack, out);
                }
            }
        }
        if let Some(ov) = &obj.object {
            for (key, prop) in &ov.properties {
                collect_enum_paths(prop, root, &join_key(path, key), stack, out);
            }
            if let Some(additional) = &ov.additional_properties {
                collect_enum_paths(additional, root, &join_key(path, "*"), stack, out);
            }
        }
        if let Some(av) = &obj.array
            && let Some(SingleOrVec::Single(item)) = &av.items
        {
            collect_enum_paths(item, root, &format!("{path}[]"), stack, out);
        }
    }

    fn all_enum_paths() -> Vec<(String, String)> {
        let root = config_schema();
        let mut out = Vec::new();
        collect_enum_paths(
            &Schema::Object(root.schema.clone()),
            root,
            "",
            &mut Vec::new(),
            &mut out,
        );
        out
    }

    /// Every definition carrying the marker (i.e. every `config_enum!` type
    /// schemars pulled into the `Config` schema).
    fn marked_definitions() -> Vec<String> {
        config_schema()
            .definitions
            .iter()
            .filter(
                |(_, s)| matches!(s, Schema::Object(o) if o.extensions.contains_key(ENUM_MARKER)),
            )
            .map(|(n, _)| n.clone())
            .collect()
    }

    // ---- completeness gate -------------------------------------------------

    /// The "impossible to miss" cross-check: every `config_enum!` type that
    /// enters the `Config` schema must be REACHED by the walker at >= 1 key
    /// path. If a future enum lands inside a container shape the walker can't
    /// traverse, this fails loudly.
    #[test]
    fn every_marked_definition_is_reachable() {
        let pairs = all_enum_paths();
        let reached: std::collections::BTreeSet<&str> =
            pairs.iter().map(|(_, n)| n.as_str()).collect();
        for def in marked_definitions() {
            assert!(
                reached.contains(def.as_str()),
                "config_enum {def} is in the schema but the validate walker \
                 never reaches it — container shape not traversed?"
            );
        }
    }

    /// Guard against an enum silently dropping OUT of the schema (which would
    /// make the reachability gate above pass vacuously): pin the count of
    /// marked definitions. 62 `config_enum!` invocations exist; `ShareReach`
    /// is intentionally absent (no config.toml key — CLI/runtime vocabulary
    /// only), so 61 must be present, and `ShareReach` explicitly must not be.
    ///
    /// This count MUST stay platform-independent: never `#[cfg(...)]`- or
    /// feature-gate a `config_enum!`-typed config *field*, or this pin passes on
    /// one platform and fails on another (e.g. the Windows CI leg) with a
    /// message that reads like a coverage regression. If a platform-specific
    /// config field is ever unavoidable, switch this pin to a per-platform
    /// expected `BTreeSet` built with the same cfg gates.
    #[test]
    fn marked_definition_count_is_pinned() {
        let defs = marked_definitions();
        assert!(
            !defs.iter().any(|d| d == "ShareReach"),
            "ShareReach grew a config key — remove it from the exclusion \
             list and bump the pinned count"
        );
        // 61 → 65: `[pr_queue]` added PrMergeMode, PrMergeMethod, PrAutoEnqueue,
        // and PrWatchKind, all strict-checked by construction via the marker.
        // 65 → 68: `[calendar]` added CalendarProviderKind, WeekStart and
        // TimeFormat. (IANA zone names deliberately are NOT a `config_enum!` —
        // ~600 values would bloat the schema and rot with each tzdb release, so
        // they are validated against the bundled database in
        // `config_calendar::validate_calendar` instead.)
        // 68 → 69: `[git] backend` (GitBackendKind) — the git read engine is
        // config-selected (provider-seams). 69 → 70: `[editor] open_in`
        // (EditorOpenIn) — the editor seam. 70 → 71: `[sandbox] on_dormant`
        // (OnDormant) — what to do when a container runtime is installed but
        // not running. 71 → 73 (THE-66): `[credentials.ssh] managed_key_scope`
        // (ManagedKeyScope) and `[identities.<name>.signing] format`
        // (SigningFormat) — the credential broker's key-custody + signing enums.
        // 73 → 74: `[notifications.push] kind` (PushKind) — the push-to-phone
        // outbound delivery provider seam.
        assert_eq!(
            defs.len(),
            74,
            "config_enum definitions in the Config schema changed; update the \
             pin (and the exclusion note) deliberately: {defs:?}"
        );
    }

    // ---- path spot-checks --------------------------------------------------

    #[test]
    fn walker_reaches_legacy_and_previously_uncovered_paths() {
        let pairs = all_enum_paths();
        let has = |path: &str, name: &str| {
            assert!(
                pairs.iter().any(|(p, n)| p == path && n == name),
                "expected ({path}, {name}) reachable; got {pairs:#?}"
            );
        };
        // All 16 legacy hand-checked paths.
        has("picker", "Picker");
        has("worktree_mode", "WorktreeMode");
        has("name_scheme", "NameScheme");
        has("sandbox.backend", "SandboxBackend");
        has("sandbox.network", "Network");
        has("sandbox.profile", "SandboxProfile");
        has("sandbox.on_missing", "OnMissing");
        has("sandbox.remote.transport", "RemoteTransport");
        has("sandbox.remote.mode", "RemoteMode");
        has("log.level", "LogLevel");
        has("log.format", "LogFormat");
        has("pins[].location", "PinLocation");
        has("theme.color", "ColorMode");
        has("theme.glyphs", "GlyphMode");
        has("merge_queue.conflict_handoff", "ConflictHandoff");
        has("merge_queue.on_landed", "OnLanded");
        // Previously-uncovered keys, one per family.
        has("lifecycle.eager", "EagerScope");
        has("sandbox.failover", "FailoverMode");
        has("toolchain.mode", "ToolchainMode");
        has("issues.provider", "IssueProviderKind");
        has("ui.sidebar_focus_detail", "FocusDetail");
        // Maps via additionalProperties.
        has("env.*.provider.connect", "ProviderConnect");
        has("host.*.reach", "HostReach");
        has("host.*.install_runtime", "InstallConsent");
        // Arrays of tables beyond pins.
        assert!(
            pairs
                .iter()
                .any(|(p, n)| p == "forges[].kind" && n == "ForgeKind"),
            "forges[].kind (ForgeKind) missing: {pairs:#?}"
        );
        // Theme + vpn + placement families.
        assert!(
            pairs
                .iter()
                .any(|(p, n)| p.starts_with("theme.mascot") && n == "MascotKind"),
            "theme mascot enum missing: {pairs:#?}"
        );
        assert!(
            pairs
                .iter()
                .any(|(p, n)| p.starts_with("sandbox.vpn.") && n == "VpnMode"),
            "sandbox.vpn mode enum missing: {pairs:#?}"
        );
        has("placement.mode", "PlacementModePref");
        has("env.*.placement", "PlacementMode");
    }

    // ---- behavior ----------------------------------------------------------

    #[test]
    fn newly_covered_keys_error_with_dotted_path_and_valid_set() {
        let errs = validate_str("[lifecycle]\neager = \"bogus\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with("lifecycle.eager: "), "{errs:?}");
        assert!(errs[0].contains("expected one of:"), "{errs:?}");

        let errs = validate_str("[env.foo.provider]\nconnect = \"bogus\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].starts_with("env.foo.provider.connect: "),
            "{errs:?}"
        );

        let errs = validate_str("[[forges]]\nkind = \"bogus\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with("forges[0].kind: "), "{errs:?}");

        let errs = validate_str("[host.box]\nreach = \"bogus\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with("host.box.reach: "), "{errs:?}");
    }

    #[test]
    fn reserved_kinds_fail_strict_validation_by_name() {
        // Canonical spelling.
        let errs = validate_str("[ci]\nprovider = \"drone\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with("ci.provider: "), "{errs:?}");
        assert!(errs[0].contains("\"drone\" is reserved"), "{errs:?}");
        // Every reserved kind in the tree is rejected, implemented ones pass.
        for (body, reserved) in [
            ("[[forges]]\nkind = \"forgejo\"\n", true),
            ("[[forges]]\nkind = \"gitea\"\n", true),
            ("[[forges]]\nkind = \"github\"\n", false),
            ("[media]\nbackend = \"jellyfin\"\n", true),
            ("[media]\nbackend = \"mpv\"\n", false),
            ("[sandbox]\nbackend = \"wsl\"\n", true),
            ("[sandbox]\nbackend = \"podman\"\n", false),
            ("[ci]\nprovider = \"argo\"\n", true),
            ("[ci]\nprovider = \"gitlab\"\n", false),
        ] {
            let errs = validate_str(body);
            assert_eq!(!errs.is_empty(), reserved, "{body:?} → {errs:?}");
            if reserved {
                assert!(errs[0].contains("reserved"), "{errs:?}");
            }
        }
    }

    #[test]
    fn schema_marker_lists_reserved_spellings() {
        let root = schemars::schema_for!(crate::config::Config);
        let def = root
            .definitions
            .get("CiProviderKind")
            .expect("CiProviderKind def");
        let Schema::Object(obj) = def else {
            panic!("not an object")
        };
        let marker = &obj.extensions[ENUM_MARKER];
        let reserved: Vec<&str> = marker["reserved"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(reserved, ["drone", "woodpecker", "jenkins", "argo"]);
        // Enums with nothing reserved still carry the (empty) list.
        let def = root.definitions.get("Picker").expect("Picker def");
        let Schema::Object(obj) = def else {
            panic!("not an object")
        };
        assert_eq!(
            obj.extensions[ENUM_MARKER]["reserved"],
            serde_json::json!([])
        );
    }

    #[test]
    fn unknown_keys_fail_strict_validation_with_a_hint() {
        let errs = validate_str("[sandbox]\nenabeld = true\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0].starts_with("sandbox.enabeld: unknown key"),
            "{errs:?}"
        );
        assert!(errs[0].contains("did you mean `enabled`"), "{errs:?}");
        // Top level, no close match.
        let errs = validate_str("zzz_not_a_key = 1\n");
        assert_eq!(errs, vec!["zzz_not_a_key: unknown key".to_string()]);
        // Map tables accept any name (the name IS the key) — and their
        // children are still checked.
        assert!(validate_str("[env.anything]\nhost = \"box\"\n").is_empty());
        let errs = validate_str("[env.anything]\nhots = \"box\"\n");
        assert!(
            errs[0].contains("env.anything.hots: unknown key"),
            "{errs:?}"
        );
        assert!(errs[0].contains("did you mean `host`"), "{errs:?}");
        // Legacy sections the loader already warns about stay quiet here.
        assert!(validate_str("[llm_proxy]\nmodel = \"x\"\n").is_empty());
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(
            nearest_key("worktreesdir", ["worktrees_dir", "other"].into_iter()),
            Some("worktrees_dir")
        );
        assert_eq!(nearest_key("totally", ["worktrees_dir"].into_iter()), None);
    }

    #[test]
    fn aliases_and_bool_failover_stay_clean() {
        // Alias accepted exactly like from_str_validated.
        assert!(validate_str("[sandbox]\nprofile = \"guarded\"\n").is_empty());
        // Bool-typed failover is legal (de_failover accepts bool OR string) —
        // the walker only inspects string values.
        assert!(validate_str("[sandbox]\nfailover = true\n").is_empty());
        let errs = validate_str("[sandbox]\nfailover = \"bogus\"\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].starts_with("sandbox.failover: "), "{errs:?}");
        // A wrong non-string type for a plain enum key is still caught — by
        // the wholesale Config parse, not the walker.
        let errs = validate_str("picker = 3\n");
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("rejected on load"), "{errs:?}");
    }

    /// Walker matching == `from_str_validated` matching by construction: both
    /// lowercase+trim the input, so every canonical value and alias must
    /// already be its own normalized form.
    #[test]
    fn canonical_values_and_aliases_are_normalization_stable() {
        for name in marked_definitions() {
            let Some(Schema::Object(obj)) = config_schema().definitions.get(&name) else {
                unreachable!()
            };
            for v in obj.enum_values.iter().flatten() {
                let s = v.as_str().expect("canonical values are strings");
                assert_eq!(s, s.trim().to_ascii_lowercase(), "{name}: {s:?}");
            }
            let marker = &obj.extensions[ENUM_MARKER];
            for a in marker["aliases"].as_array().into_iter().flatten() {
                let s = a.as_str().expect("aliases are strings");
                assert_eq!(s, s.trim().to_ascii_lowercase(), "{name}: {s:?}");
            }
        }
    }

    /// No false positives on every documented key: the shipped example config
    /// must validate clean.
    #[test]
    fn example_config_validates_clean() {
        let body = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config/config.toml.example"
        ));
        let errs = validate_str(body);
        assert!(errs.is_empty(), "{errs:#?}");
    }

    /// Pins the published-schema fix: `config schema` / the MCP feed now
    /// advertise the canonical strings `from_str_validated` accepts, not the
    /// Rust variant identifiers.
    #[test]
    fn published_schema_advertises_canonical_strings() {
        let picker = schemars::schema_for!(crate::config::Picker);
        let values: Vec<&str> = picker
            .schema
            .enum_values
            .iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(values, ["auto", "gum", "fzf", "select"]);

        let wm = schemars::schema_for!(crate::config::WorktreeMode);
        let values: Vec<&str> = wm
            .schema
            .enum_values
            .iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(values.contains(&"in_repo"), "{values:?}");
        assert!(!values.contains(&"InRepo"), "{values:?}");
    }
}
