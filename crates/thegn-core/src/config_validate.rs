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
//! `[keybinds]` map has an explicit schema preserving free-form chord strings.

use std::sync::OnceLock;

use schemars::schema::{InstanceType, RootSchema, Schema, SchemaObject, SingleOrVec};

use crate::config::Config;

/// Schema-extension key `config_enum!` stamps on every enum it defines; the
/// value is `{ "kind": <human kind>, "aliases": [<accepted aliases>] }`.
pub const ENUM_MARKER: &str = "x-thegn-enum";

/// Generated once. Schemars may construct environment-derived serde defaults
/// for schema metadata; these are not installed as an effective Config.
fn config_schema() -> &'static RootSchema {
    static SCHEMA: OnceLock<RootSchema> = OnceLock::new();
    SCHEMA.get_or_init(|| schemars::schema_for!(Config))
}

/// Strictly validate a raw `config.toml` body, collecting human-readable errors
/// for `config validate` (the only place a bad value is treated as an error
/// rather than warned-and-defaulted). Returns the list of problems (empty = ok).
pub fn validate_str(body: &str) -> Vec<String> {
    validate_diagnostics(body)
        .into_iter()
        .filter(|diagnostic| diagnostic.severity == ValidationSeverity::Error)
        .map(|diagnostic| diagnostic.message)
        .collect()
}

/// Severity is data, never inferred from a human-readable message prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationDiagnostic {
    pub severity: ValidationSeverity,
    pub message: String,
}

/// How a schema walk treats a key the schema does not know.
///
/// One shared walk serves several callers with different stakes, so the
/// policy is chosen PER CALL SITE (never inside the walk): the trusted
/// configuration layers may relax, while the repo overlay, the host-row
/// decoder and every other consumer keep refusing every unknown key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnknownKeys {
    /// Every unknown key is an error (the default everywhere).
    Reject,
    /// An unknown key under a security-relevant table is an error; anywhere
    /// else it is an ignorable warning. This is what the admitted
    /// configuration layers use: one `~/.config/thegn/config.toml` is shared
    /// by builds of different ages, so a key a newer build added must not
    /// brick an older one — but a key an older build does not understand in
    /// a table that grants authority must still refuse, because "ignored"
    /// there can mean "policy silently not applied".
    RejectSecurityRelevant,
}

/// Tables whose unknown keys refuse admission: anything that can execute a
/// process, reach a remote, hold or select credentials, gate schema
/// migrations, or set sandbox/placement/queue policy. Everything outside this
/// list is presentation, telemetry or layout, where an unknown key is
/// ignorable.
///
/// This is a DENYLIST on purpose: the whole point of the relaxation is that a
/// table a NEWER build added must not brick an older build sharing one
/// `config.toml`, and an allowlist would refuse exactly that case. The cost
/// is that the list rots as tables are added, so
/// `security_relevant_roots_cover_every_executing_or_credential_table`
/// derives the expectation from the generated schema and fails when a new
/// root carries a command/argv, a secret ref, or a URL/host field.
///
/// Names are SCHEMA property names, not the spellings a user may write:
/// `[workspace.<slug>]` is normalized to `[project.<slug>]` before any walk
/// (`config_compat::normalize_project_tables`, `#[serde(rename = "project",
/// alias = "workspace")]`), so the protected name here is `project`. Keep
/// this list, `docs/ARCHITECTURE.md` and `docs/help/configuration.md` in
/// sync.
const SECURITY_RELEVANT_ROOTS: &[&str] = &[
    "accounts",
    "actions",
    "agents",
    "apps",
    "automations",
    "autopilot",
    "bundle",
    "calendar",
    "ci",
    "clipboard",
    "credentials",
    "daemon",
    "database",
    "disk",
    // The drawer's scoped entries carry commands.
    "drawer",
    "editor",
    "env",
    "forges",
    "forward",
    "git",
    "git_commands",
    "hooks",
    "host",
    "host_discovery",
    "identities",
    // Inbox accounts run commands and carry credential refs (THE-2214).
    "inbox",
    "issues",
    // Selects the env that executes `thegn job run` recipes remotely.
    "jobs",
    "lead",
    "lifecycle",
    "limits",
    "lsp",
    "managed_tools",
    "mcp",
    "mcp_proxy",
    "mcp_servers",
    // Player control commands plus a remote library (url + token).
    "media",
    "merge_queue",
    "metrics",
    "model_proxy",
    "network",
    "notifications",
    "observe",
    "pins",
    "pipeline",
    "placement",
    "plugins",
    "pr",
    "pr_queue",
    "presets",
    // The per-project overlay: the schema property is `project`; `workspace`
    // is the legacy spelling normalization rewrites before any walk. Its
    // subtree carries accounts, hooks, sandbox mounts, the queues, ci,
    // autopilot, the MCP scope ceiling, env bundles, git and editor.
    "project",
    "profiles",
    // Scalar roots, not tables: listing them has no nested effect, but it
    // makes a TYPO of them (`worktrees_dirr`) refuse instead of silently
    // relocating every worktree to the default root.
    "projects_dir",
    "remote",
    "sandbox",
    "secrets",
    "serve",
    "share",
    "skills",
    // Custom stat collectors run commands.
    "stats",
    "tasks",
    "toolchain",
    "tools",
    "usage",
    "voice",
    "watch",
    "weather",
    "workspace",
    "worktree_templates",
    "worktrees_dir",
    "zone",
];

/// Whether `path` (a dotted schema path such as `sandbox.mounts[0].source`)
/// names a security-relevant table. A bare root key (`picker`) is not one:
/// an unknown ROOT key is exactly the "newer build added a table" case.
pub fn is_security_relevant_path(path: &str) -> bool {
    let root = path_root(path);
    !root.is_empty() && path.len() > root.len() && SECURITY_RELEVANT_ROOTS.contains(&root)
}

fn path_root(path: &str) -> &str {
    path.split(['.', '['])
        .next()
        .unwrap_or_default()
        .trim_end_matches(']')
}

/// Whether an unknown key must refuse: either it sits under a
/// security-relevant table, or the walk's nearest-key hint says the user
/// meant one. The hint is how a TYPO'd security table (`[sandboxx]`,
/// `[metric]`) is caught instead of being dropped with its whole contents.
///
/// The trade-off is real and irreducible, and it is deliberate: a typo and a
/// legitimately NEW table one or two edits from a security-relevant name are
/// the same shape, so this build also refuses a new top-level table spelled
/// `[hosts]`, `[bundles]`, `[zones]`, `[secret]`, `[agent]`, `[stat]`, …
/// This schema already has such neighbours — `profile` and `profiles` differ
/// by one edit and one of them is security-relevant — so had `profiles`
/// arrived later, an older build would refuse it. The remedy for an operator
/// is to rename or remove the key; `docs/help/configuration.md` says so. To
/// drop the trade-off, delete the top-level (`None`) branch below and keep
/// only the nested rule: typos inside a security table would still refuse.
fn unknown_key_refuses(path: &str, hint: Option<&str>) -> bool {
    if is_security_relevant_path(path) {
        return true;
    }
    let Some(hint) = hint else { return false };
    match path.rsplit_once('.') {
        // A nested typo: judge the corrected path.
        Some((parent, _)) => is_security_relevant_path(&format!("{parent}.{hint}")),
        // A top-level typo: the corrected ROOT decides.
        None => SECURITY_RELEVANT_ROOTS.contains(&hint),
    }
}

/// The stable marker the walk uses for an unknown key.
const UNKNOWN_KEY: &str = ": unknown key";
/// How the walk renders its nearest-key hint: `(did you mean \`x\`?)`.
const HINT_PREFIX: &str = "(did you mean `";

/// Split walk errors into (errors, ignorable warnings) under `policy`.
pub(crate) fn split_unknown_keys(
    messages: Vec<String>,
    policy: UnknownKeys,
) -> (Vec<String>, Vec<String>) {
    if policy == UnknownKeys::Reject {
        return (messages, Vec::new());
    }
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for message in messages {
        match message.split_once(UNKNOWN_KEY) {
            Some((path, tail)) => {
                let hint = tail
                    .split_once(HINT_PREFIX)
                    .and_then(|(_, rest)| rest.split_once('`'))
                    .map(|(hint, _)| hint);
                if unknown_key_refuses(path, hint) {
                    errors.push(message);
                } else {
                    warnings.push(format!("{message} (ignored by this build)"));
                }
            }
            None => errors.push(message),
        }
    }
    (errors, warnings)
}

/// Validate the same normalized document that runtime loading accepts, retaining
/// advisory compatibility diagnostics separately from actual admission errors.
/// Canonical/legacy collisions remain warnings: normalization deliberately
/// chooses the canonical value at runtime and this API preserves that policy.
pub fn validate_diagnostics(body: &str) -> Vec<ValidationDiagnostic> {
    validate_diagnostics_with_policy(body, UnknownKeys::Reject)
}

/// [`validate_diagnostics`] under an explicit unknown-key policy. The trusted
/// layers (main config, selected profile overlay) pass
/// [`UnknownKeys::RejectSecurityRelevant`] so `config validate` reports
/// exactly what admission does; the repo overlay keeps `Reject`.
pub fn validate_diagnostics_with_policy(
    body: &str,
    policy: UnknownKeys,
) -> Vec<ValidationDiagnostic> {
    let normalized = match crate::config_compat::normalize(body) {
        Ok(value) => value,
        Err(error) => {
            return vec![ValidationDiagnostic {
                severity: ValidationSeverity::Error,
                message: format!("TOML syntax error: {error}"),
            }];
        }
    };
    let mut diagnostics: Vec<_> = normalized
        .diagnostics
        .iter()
        .map(|message| ValidationDiagnostic {
            severity: ValidationSeverity::Warning,
            message: message.clone(),
        })
        .collect();
    let (errors, ignorable) = split_unknown_keys(validate_normalized(&normalized.body), policy);
    diagnostics.extend(ignorable.into_iter().map(|message| ValidationDiagnostic {
        severity: ValidationSeverity::Warning,
        message,
    }));
    diagnostics.extend(errors.into_iter().map(|message| ValidationDiagnostic {
        severity: ValidationSeverity::Error,
        message,
    }));
    if let Ok(mut cfg) = toml::from_str::<Config>(&normalized.body) {
        if let Ok(raw) = toml::from_str::<serde_json::Value>(&normalized.body) {
            crate::config_env_tables::mark_model_credentials_explicit(&mut cfg, &raw);
        }
        diagnostics.extend(float_range_warnings(&cfg).into_iter().map(|message| {
            ValidationDiagnostic {
                severity: ValidationSeverity::Warning,
                message,
            }
        }));
        diagnostics.extend(sidebar_detail_warnings(&cfg).into_iter().map(|message| {
            ValidationDiagnostic {
                severity: ValidationSeverity::Warning,
                message,
            }
        }));
        diagnostics.extend(list_value_warnings(&cfg).into_iter().map(|message| {
            ValidationDiagnostic {
                severity: ValidationSeverity::Warning,
                message,
            }
        }));
        diagnostics.extend(
            enum_value_warnings(&cfg)
                .into_iter()
                .chain(alert_order_warnings(&cfg))
                .chain(notification_value_warnings(&cfg))
                .map(|message| ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message,
                }),
        );
        if cfg.voice.kind == crate::config::VoiceKind::Http
            && let Err(message) = cfg.voice.validate_credential_transport()
        {
            diagnostics.push(ValidationDiagnostic {
                severity: ValidationSeverity::Error,
                message,
            });
        }
        for (index, mount) in cfg.sandbox.mounts.iter().enumerate() {
            if let Err(error) = crate::sandbox_mounts::parse_mount_checked(mount) {
                diagnostics.push(ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message: format!(
                        "sandbox.mounts[{index}] `{mount}`: {error} — the entry is ignored"
                    ),
                });
            }
        }
        for (slug, workspace) in &cfg.workspace {
            for (index, mount) in workspace.sandbox_mounts.iter().enumerate() {
                if let Err(error) = crate::sandbox_mounts::parse_mount_checked(mount) {
                    diagnostics.push(ValidationDiagnostic {
                        severity: ValidationSeverity::Warning,
                        message: format!(
                            "project.{slug}.sandbox_mounts[{index}] `{mount}`: {error} — the entry is ignored"
                        ),
                    });
                }
            }
        }
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|value| !value.is_empty())
            .map(std::path::PathBuf::from);
        diagnostics.extend(
            mount_exposure_warnings(&cfg, runtime_dir.as_deref(), &crate::util::home())
                .into_iter()
                .map(|message| ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message,
                }),
        );
        if let Some(message) = cfg.sandbox.warm_direnv.deprecation_warning() {
            diagnostics.push(ValidationDiagnostic {
                severity: ValidationSeverity::Warning,
                message: message.to_string(),
            });
        }
        diagnostics.extend(
            crate::config_calendar::display_warnings(&cfg.calendar)
                .into_iter()
                .map(|message| ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message,
                }),
        );
        diagnostics.extend(
            crate::provider_admission::unenforced_lifetime_warnings(&cfg)
                .into_iter()
                .map(|message| ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message,
                }),
        );
        diagnostics.extend(
            crate::provider_admission::provider_table_warnings(&cfg)
                .into_iter()
                .map(|message| ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message,
                }),
        );
        diagnostics.extend(sandbox_git_warnings(&cfg).into_iter().map(|message| {
            ValidationDiagnostic {
                severity: ValidationSeverity::Warning,
                message,
            }
        }));
        diagnostics.extend(network_audit_warnings(&cfg).into_iter().map(|message| {
            ValidationDiagnostic {
                severity: ValidationSeverity::Warning,
                message,
            }
        }));
        diagnostics.extend(
            cfg.placement
                .fleet_budget_warnings()
                .into_iter()
                .map(|message| ValidationDiagnostic {
                    severity: ValidationSeverity::Warning,
                    message,
                }),
        );
    }
    diagnostics
}

/// `network_audit` subscribes the podman `network` event stream (container
/// network attach/detach). Warn when no selectable backend — the explicit
/// `backend`, or every `backend_chain` entry under `auto` — implements the
/// events op, because then nothing is ever audited.
fn network_audit_warnings(cfg: &Config) -> Vec<String> {
    use crate::sandbox::Backend;
    if !cfg.sandbox.network_audit {
        return Vec::new();
    }
    let selectable: Vec<Backend> = match Backend::from_config(cfg.sandbox.backend) {
        Some(backend) => vec![backend],
        None => cfg
            .sandbox
            .backend_chain
            .iter()
            .filter_map(|name| Backend::parse(name))
            .collect(),
    };
    if selectable
        .iter()
        .any(|backend| backend.profile().events == crate::sandbox_events::EventsCap::Yes)
    {
        return Vec::new();
    }
    let names: Vec<&str> = selectable.iter().map(|backend| backend.label()).collect();
    vec![format!(
        "sandbox.network_audit = true, but no selectable backend ({}) delivers the podman \
         `network` event stream, so nothing is audited; network_audit records container network \
         attach/detach on podman backends only and is not a DNS-query or connection log",
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    )]
}

/// THE-2021: `[sandbox.git]` policy warnings for the global table and every
/// `[profiles.<n>.sandbox.git]` / `[env.<n>.sandbox.git]` overlay (each
/// validated as merged onto the global table, labelled with its layer). An
/// overlay repeats only warnings the global table does not already raise.
fn sandbox_git_warnings(cfg: &Config) -> Vec<String> {
    use crate::config_sandbox::git_policy::validate;
    let global = validate(&cfg.sandbox.git);
    let mut layers = Vec::new();
    for (name, profile) in &cfg.profiles {
        if let Some(git) = &profile.sandbox.git {
            layers.push((format!("profiles.{name}."), cfg.sandbox.git.merged(git)));
        }
    }
    for (name, env) in &cfg.env {
        if let Some(git) = &env.sandbox.git {
            layers.push((format!("env.{name}."), cfg.sandbox.git.merged(git)));
        }
    }
    let mut out = global.clone();
    for (prefix, git) in layers {
        out.extend(
            validate(&git)
                .into_iter()
                .filter(|message| !global.contains(message))
                .map(|message| format!("{prefix}{message}")),
        );
    }
    out
}

/// THE-2009: warn on sandbox mounts that expose `$XDG_RUNTIME_DIR` or make
/// `~/.claude-profiles` writable, across every layer that declares mounts.
/// The runtime dir and home are injected so tests never read the ambient env.
fn mount_exposure_warnings(
    cfg: &Config,
    runtime_dir: Option<&std::path::Path>,
    home: &std::path::Path,
) -> Vec<String> {
    let mut labelled: Vec<(String, String)> = Vec::new();
    let mut add = |prefix: &str, mounts: &[String]| {
        labelled.extend(
            mounts
                .iter()
                .enumerate()
                .map(|(index, mount)| (format!("{prefix}[{index}]"), mount.clone())),
        );
    };
    add("sandbox.mounts", &cfg.sandbox.mounts);
    for (name, profile) in &cfg.profiles {
        if let Some(mounts) = &profile.sandbox.mounts {
            add(&format!("profiles.{name}.sandbox.mounts"), mounts);
        }
    }
    for (name, env) in &cfg.env {
        if let Some(mounts) = &env.sandbox.mounts {
            add(&format!("env.{name}.sandbox.mounts"), mounts);
        }
    }
    for (slug, workspace) in &cfg.workspace {
        add(
            &format!("project.{slug}.sandbox_mounts"),
            &workspace.sandbox_mounts,
        );
    }
    crate::sandbox_runtime_escape::exposure_warnings(&labelled, runtime_dir, home)
}

fn validate_normalized(body: &str) -> Vec<String> {
    let mut errs = Vec::new();
    let val: toml::Value = match body.parse() {
        Ok(v) => v,
        Err(e) => return vec![format!("TOML syntax error: {e}")],
    };
    // The wholesale check load_layered enforces: if the body fails to
    // deserialize into `Config`, the entire file is discarded for defaults.
    // This catches shape/type errors; the schema walk below catches the
    // warn-and-default enum values `Deserialize` never rejects.
    let load_error = match toml::from_str::<Config>(body) {
        Err(e) => Some(e),
        // Templates are strings as far as the schema is concerned, so their
        // placeholders can only be checked once the file has deserialized.
        Ok(cfg) => {
            errs.extend(typed_semantic_errors(&cfg, SemanticMode::AllDiagnostics));
            None
        }
    };
    let val = serde_json::to_value(val).expect("TOML values are JSON-compatible");
    let root = config_schema();
    let before_schema = errs.len();
    walk_object(&root.schema, root, &val, "", &mut errs, true);
    if let Some(error) = load_error {
        let type_errors: Vec<String> = errs[before_schema..]
            .iter()
            .filter(|message| message.contains(": expected "))
            .cloned()
            .collect();
        errs.truncate(before_schema);
        if type_errors.is_empty() {
            errs.push(format!("config would be rejected on load: {error}"));
        } else {
            errs.push(format!(
                "config would be rejected on load: {error}; {}",
                type_errors.join("; ")
            ));
        }
    }
    errs
}

/// Existing typed checks in their original diagnostic order. Checked host
/// composition stops after one failed batch; legacy document validation collects
/// every batch. Individual validators retain their existing internal behavior.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SemanticMode {
    AllDiagnostics,
    StopOnError,
}

pub(crate) fn typed_semantic_errors(cfg: &Config, mode: SemanticMode) -> Vec<String> {
    #[cfg(test)]
    semantic_observation::record("semantic_start");
    let mut errs = Vec::new();
    macro_rules! stop {
        () => {
            if mode == SemanticMode::StopOnError && !errs.is_empty() {
                return errs;
            }
        };
    }
    macro_rules! batch {
        ($errors:expr $(,)?) => {{
            errs.extend($errors);
            stop!();
        }};
    }
    let max_load = cfg.sandbox.limits.max_load;
    if !max_load.is_finite() || max_load < 0.0 {
        errs.push("sandbox.limits.max_load must be a finite non-negative load average".to_string());
        stop!();
    }
    check_templates(cfg, &mut errs);
    stop!();
    if let Some(name) = cfg.default_folder.as_deref()
        && name.trim().is_empty()
    {
        errs.push("default_folder must not be empty or whitespace-only".into());
        stop!();
    }
    batch!(crate::custom_cmd::validate_commands(&cfg.git_commands));
    batch!(cfg.autopilot.validate("autopilot"));
    batch!(cfg.jobs.validate("jobs", &cfg.env));
    for (slug, ws) in &cfg.workspace {
        batch!(
            ws.autopilot
                .validate(&format!("workspace.{slug}.autopilot")),
        );
    }
    // `[[presets]]` semantic checks (empty preset, template `preset`
    // exclusivity) — strings to the schema, so only checkable post-parse.
    batch!(crate::config_presets::validate_presets(cfg));
    // `[[pipeline.stages]]` semantic checks (names, resolvable agents,
    // concurrency, `next` targets and cycles). Structure only — thegn
    // validates the org chart it will never execute.
    batch!(crate::config_pipeline::validate_pipeline(cfg));
    // Compatibility validation entry point; completion policy is generated
    // by the host at runtime and is not inferred from configurable task text.
    batch!(crate::config_pipeline::validate_stage_contracts(cfg));
    // `model` / `env` on [[agents]]/[[tools]] and stage overrides: a
    // model must land on a harness with a model flag, env keys must be
    // exportable names.
    batch!(crate::agent_task::validate_agent_models(cfg));
    // `[apps]` ids are a public cross-surface registry. Keep the diagnostic
    // actionable here, where the typed config (including enable switches) is
    // available.
    batch!(cfg.apps.validate(cfg.observe.enabled));
    // Skill names and directory-list syntax are a config-boundary
    // concern. Directory existence/discovery stays at the host edge.
    batch!(cfg.skills.validate());
    batch!(crate::config_drawer::validate_drawer_config(cfg));
    check_serve(cfg, &mut errs);
    stop!();
    // THE-515: trusted `[workspace.<key>]` keys must be unambiguous slugs —
    // a non-normalized or colliding key is refused by the runtime selector,
    // so admission must refuse it visibly before any repo-scoped effect.
    batch!(crate::workspace_overlay::validate(&cfg.workspace));
    // IANA zone names can't be a `config_enum!` (~600 of them, and the
    // list rots with each tzdb release), so `[calendar]` is checked
    // against the bundled database here instead — with a did-you-mean.
    batch!(crate::config_calendar::validate_calendar(&cfg.calendar));
    // `[weather]`'s enum spellings are strict-checked by the schema
    // walker; what it can't see are the interval relationships (a hard
    // expiry at or under the stale threshold hides the widget before it
    // can ever render stale) and the SecretRef custody rule on
    // `api_key`.
    batch!(crate::config_weather::validate_weather(&cfg.weather));
    // `[inbox]` (THE-2214): account names, reserved kinds, per-kind
    // required fields, the SecretRef custody rule on `token`/`password`,
    // the https rule on `base_url`, and the item/timeout caps.
    batch!(cfg.inbox.validate());
    // `[[lsp.servers]]` is a registry: a non-built-in key must declare
    // extensions, and an extension may not be claimed by two entries.
    batch!(crate::lsp_registry::validate_servers(&cfg.lsp.servers));
    // The push command inbox: enabling it demands a SecretRef secret,
    // a non-empty allow list of known non-admin capabilities, and valid
    // scopes — a subscribed-but-inert inbox is not a valid state.
    batch!(cfg.notifications.push.inbox.validate_errors());
    // The crash-forwarding sink is a reserved provider-seam kind — a
    // non-empty value is rejected (not silently ignored).
    if let Err(e) = cfg.diagnostics.validate_crash_sink() {
        errs.push(e);
        stop!();
    }
    batch!(cfg.notifications.push.validate_errors());
    let sink_names: std::collections::BTreeSet<String> = cfg
        .notifications
        .push
        .effective_sinks()
        .into_iter()
        .map(|sink| sink.name.trim().to_string())
        .collect();
    for (index, rule) in cfg.notifications.rules.iter().enumerate() {
        for channel in rule.route.iter().flatten() {
            let Some(name) = channel.trim().strip_prefix("push:") else {
                continue;
            };
            let name = name.trim();
            if name.is_empty() || !sink_names.contains(name) {
                errs.push(format!(
                    "notifications.rules[{index}].route names unknown push sink {name:?}"
                ));
                stop!();
            }
        }
    }
    // `[notifications]` live-agent signatures must be non-empty and
    // bounded; otherwise an empty substring would match every line.
    batch!(cfg.notifications.validate());
    batch!(cfg.automations.validate());
    if let Some(lead) = &cfg.lead {
        batch!(lead.validate(&cfg.agents));
    }
    batch!(cfg.mcp_proxy.validate(&cfg.mcp_servers));
    for (name, profile) in &cfg.profiles {
        if profile.automations.is_empty() {
            continue;
        }
        #[cfg(test)]
        semantic_observation::record("profile_clone");
        let mut effective = cfg.automations.clone();
        profile.automations.clone().apply(&mut effective);
        batch!(
            effective
                .validate()
                .into_iter()
                .map(|error| format!("profiles.{name}: {error}")),
        );
    }
    // Sound references and kind selectors use a free-form map, so the
    // schema walker cannot validate their keys or values.
    batch!(cfg.notifications.validate_sound());
    for (profile, profile_cfg) in &cfg.profiles {
        batch!(
            profile_cfg
                .notifications
                .validate_sound(&format!("profiles.{profile}.notifications.sound")),
        );
    }
    // `[model_proxy]` — SecretRef-only keys, routes referencing declared
    // providers, aliases naming real routes. Only when enabled.
    batch!(cfg.model_proxy.validate());
    batch!(cfg.ci.validate());
    batch!(crate::provider_admission::validate_config(cfg));
    // `[placement.fleet_budget]` is a free-form provider map: refuse a
    // negative or non-finite limit (warnings ride `validate_diagnostics`).
    batch!(cfg.placement.fleet_budget_errors());
    errs
}

/// Inclusive ranges for float keys whose schema carries no `maximum` (a schema
/// maximum is reported as a duration by `check_number`). `crit_percent` below
/// `warn_percent` is deliberately allowed: the usage tone treats it as warn.
///
/// These are display settings whose consumers already tolerate any value, so
/// an out-of-range or non-finite value is a WARNING (named with its value and
/// range) and never refuses startup: configs that loaded before keep loading.
///
/// The six `activity.*_secs` floats are not listed: their schema already
/// carries `range(max = MAX_DURATION_SECS)`, so `check_number` rejects a
/// negative, non-finite or over-max value as a duration error.
pub const FLOAT_KEY_RANGES: [(&str, f64, f64); 5] = [
    ("media.volume_step", 0.0, 1.0),
    ("usage.warn_percent", 0.0, 100.0),
    ("usage.crit_percent", 0.0, 100.0),
    ("activity.cpu_percent", 0.0, 100.0),
    ("activity.runaway_core_fraction", 0.0, 1.0),
];

/// Inclusive range for each `stats.refresh_rates` entry (seconds). The keybind
/// consumer floors an entry at 0.5, so a smaller one is silently raised; the
/// ceiling is the shared duration cap. Out-of-range and non-finite entries
/// WARN (one per entry, named by index) and are kept; an empty list is valid
/// (the consumer then skips cycling).
pub const REFRESH_RATE_RANGE: (f64, f64) = (0.5, crate::time_policy::MAX_DURATION_SECS as f64);

pub(crate) fn float_range_warnings(cfg: &Config) -> Vec<String> {
    let values = [
        (cfg.media.volume_step, cfg.media.volume_step.to_string()),
        (
            f64::from(cfg.usage.warn_percent),
            cfg.usage.warn_percent.to_string(),
        ),
        (
            f64::from(cfg.usage.crit_percent),
            cfg.usage.crit_percent.to_string(),
        ),
        (
            cfg.activity.cpu_percent,
            cfg.activity.cpu_percent.to_string(),
        ),
        (
            cfg.activity.runaway_core_fraction,
            cfg.activity.runaway_core_fraction.to_string(),
        ),
    ];
    let mut warnings: Vec<String> = FLOAT_KEY_RANGES
        .iter()
        .zip(values)
        .filter(|((_, min, max), (value, _))| {
            !(value.is_finite() && *min <= *value && *value <= *max)
        })
        .map(|((path, min, max), (_, shown))| {
            format!(
                "{path} = {shown} is outside the valid range {min}..={max} (finite numbers only); the value is kept"
            )
        })
        .collect();
    let (min, max) = REFRESH_RATE_RANGE;
    for (index, value) in cfg.stats.refresh_rates.iter().enumerate() {
        if !(value.is_finite() && min <= *value && *value <= max) {
            warnings.push(format!(
                "stats.refresh_rates[{index}] = {value} is outside the valid range {min}..={max} (finite numbers only); the value is kept"
            ));
        }
    }
    warnings
}

fn sidebar_detail_warnings(cfg: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for field in &cfg.ui.sidebar_detail_fields {
        if crate::config_ui::parse_entry(field).is_none() {
            warnings.push(format!(
                "ui.sidebar_detail_fields: unknown field {field:?}; expected a built-in name or plugin:<name>/<field>"
            ));
        }
    }
    for (key, badge) in [
        ("ui.sidebar_commit_badge", &cfg.ui.sidebar_commit_badge),
        ("ui.sidebar_behind_badge", &cfg.ui.sidebar_behind_badge),
    ] {
        if !badge.contains("{n}") {
            warnings.push(format!(
                "{key}: badge template must contain {{n}}; the default will be used"
            ));
        }
    }
    warnings
}

/// List entries the loader silently skips: DND windows and usage provider ids.
/// Warnings only (never errors); values are echoed with `{:?}` so control
/// characters in user text are escaped.
fn list_value_warnings(cfg: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for (i, value) in cfg.notifications.dnd.windows.iter().enumerate() {
        if crate::notification_route::parse_window(value).is_none() {
            warnings.push(format!(
                "notifications.dnd.windows[{i}] = {value:?} is not a window (\"HH:MM-HH:MM\" with optional weekdays); it is ignored"
            ));
        }
    }
    for (i, id) in cfg.usage.providers.iter().enumerate() {
        if id != "antigravity" && crate::account::provider(id).is_none() {
            warnings.push(format!(
                "usage.providers[{i}] = {id:?} is not a known usage provider; it is ignored"
            ));
        }
    }
    warnings
}

/// Notification string settings whose unknown values silently fall back to a
/// default. Warnings only; values are echoed with `{:?}`.
fn enum_value_warnings(cfg: &Config) -> Vec<String> {
    let mut warnings = Vec::new();
    let exit = &cfg.notifications.process_exit;
    if crate::event_bus::ProcessExitPolicy::parse_known(exit).is_none() {
        warnings.push(format!(
            "notifications.process_exit = {exit:?} is not one of off, failures, failures_and_tasks, all; the default is used"
        ));
    }
    let urgency = &cfg.notifications.desktop_min_urgency;
    if crate::event_bus::NotificationUrgency::parse_known(urgency).is_none() {
        warnings.push(format!(
            "notifications.desktop_min_urgency = {urgency:?} is not one of low, normal, critical; the default is used"
        ));
    }
    warnings
}

/// `notifications.priority` keys/values, `active_mode` naming an undefined mode,
/// and empty `agent_error_signatures` entries. Warnings only.
fn notification_value_warnings(cfg: &Config) -> Vec<String> {
    let n = &cfg.notifications;
    let mut warnings = Vec::new();
    for (kind, word) in &n.priority {
        if !crate::notification::NotificationKind::ALL
            .iter()
            .any(|k| k.as_str() == kind)
        {
            warnings.push(format!(
                "notifications.priority.{kind} is not a known notification kind; the entry is ignored"
            ));
        }
        if crate::notification::Priority::parse(word).is_none() {
            warnings.push(format!(
                "notifications.priority.{kind} = {word:?} is not one of info, notice, alert; the kind's default is used"
            ));
        }
    }
    if !n.active_mode.is_empty() && !n.modes.contains_key(&n.active_mode) {
        warnings.push(format!(
            "notifications.active_mode = {:?} does not name a key of notifications.modes",
            n.active_mode
        ));
    }
    if n.agent_error_signatures.iter().any(|s| s.trim().is_empty()) {
        warnings.push(
            "notifications.agent_error_signatures has an empty entry; it matches every line"
                .to_string(),
        );
    }
    warnings
}

/// Alert rules whose warn level can never fire before critical. Both levels
/// must be enabled (> 0); equal values warn. Warnings only.
fn alert_order_warnings(cfg: &Config) -> Vec<String> {
    let a = &cfg.stats.alerts;
    let above = [
        ("cpu", &a.cpu),
        ("mem", &a.mem),
        ("swap", &a.swap),
        ("gpu", &a.gpu),
        ("temp", &a.temp),
        ("load", &a.load),
        ("reclaim", &a.reclaim),
    ];
    let below = [
        ("disk_free", &a.disk_free),
        ("battery", &a.battery),
        ("disk_eta", &a.disk_eta),
    ];
    let mut warnings = Vec::new();
    for (name, rule) in above {
        if rule.warn > 0.0 && rule.critical > 0.0 && rule.warn >= rule.critical {
            warnings.push(format!(
                "stats.alerts.{name}: warn {} is not below critical {}; the warn level never fires first",
                rule.warn, rule.critical
            ));
        }
    }
    for (name, rule) in below {
        if rule.warn > 0.0 && rule.critical > 0.0 && rule.warn <= rule.critical {
            warnings.push(format!(
                "stats.alerts.{name}: warn {} is not above critical {}; the warn level never fires first",
                rule.warn, rule.critical
            ));
        }
    }
    warnings
}

/// Same project-specific schema semantics as document validation, including
/// its compatibility branches and flattened-map limitations.
pub(crate) fn validate_config_schema_value(value: &serde_json::Value) -> Vec<String> {
    validate_schema_value_with_root(value, config_schema())
}

/// The admitted-layer walk: unknown keys outside security-relevant tables are
/// returned as ignorable warnings instead of refusals. Only the configuration
/// layers use this; `validate_schema_value_with_root` (repo overlay, host-row
/// decoder, every other schema) still refuses every unknown key.
pub(crate) fn validate_config_layer_schema_value(
    value: &serde_json::Value,
) -> (Vec<String>, Vec<String>) {
    split_unknown_keys(
        validate_schema_value_with_root(value, config_schema()),
        UnknownKeys::RejectSecurityRelevant,
    )
}

#[cfg(test)]
pub(crate) mod semantic_observation {
    use std::cell::RefCell;
    thread_local! {
        static EVENTS: RefCell<Option<Vec<&'static str>>> = const { RefCell::new(None) };
    }
    pub(crate) fn record(event: &'static str) {
        EVENTS.with_borrow_mut(|events| {
            if let Some(events) = events {
                events.push(event);
            }
        });
    }
    pub(crate) fn capture<T>(action: impl FnOnce() -> T) -> (T, Vec<&'static str>) {
        struct Restore(Option<Vec<&'static str>>);
        impl Drop for Restore {
            fn drop(&mut self) {
                EVENTS.with_borrow_mut(|events| *events = self.0.take());
            }
        }
        let restore = Restore(EVENTS.replace(Some(Vec::new())));
        let result = action();
        let events = EVENTS.replace(None).expect("active observation");
        drop(restore);
        (result, events)
    }
}

/// Validate a format-neutral document against a config schema.  Repo-local
/// overlays use this same walker as the trusted `Config` document, but supply
/// their narrower `RepoConfigFile` schema.
pub(crate) fn validate_schema_value<T: schemars::JsonSchema>(
    value: &serde_json::Value,
) -> Vec<String> {
    let root = schemars::schema_for!(T);
    validate_schema_value_with_root(value, &root)
}

/// Reuse a generated schema for bounded batches without changing validation.
pub(crate) fn validate_schema_value_with_root(
    value: &serde_json::Value,
    root: &RootSchema,
) -> Vec<String> {
    let mut errs = Vec::new();
    walk_object(&root.schema, root, value, "", &mut errs, true);
    errs
}

/// Check every agent prompt/command template against the variables its surface
/// actually provides. A typo like `{branchh}` would otherwise reach the agent as
/// an empty expansion mid-drain; here it is a `config validate` error.
fn check_templates(cfg: &Config, errs: &mut Vec<String>) {
    use crate::agent_task::{
        COMMAND_VARS, LAND_MESSAGE_VARS, STAGE_VARS, TaskKind, validate_template,
    };

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
    check(
        "merge_queue.land_message",
        &mq.land_message,
        LAND_MESSAGE_VARS,
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

    check(
        "autopilot.agent_command",
        &cfg.autopilot.agent_command,
        COMMAND_VARS,
        true,
    );

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
        if let Some(c) = ws.autopilot.agent_command.as_deref() {
            check(
                &format!("workspace.{slug}.autopilot.agent_command"),
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
        if let Some(t) = o.land_message.as_deref() {
            check(
                &format!("workspace.{slug}.merge_queue.land_message"),
                t,
                LAND_MESSAGE_VARS,
                false,
            );
        }
    }

    // `[[pipeline.stages]] prompt` — the one template family thegn never
    // renders itself (the supervising agent does), so validate-time is the ONLY
    // place a `{typo}` can be caught before it reaches a worker as an empty
    // expansion. Not a `TaskKind`: a flat variable list keeps the check honest
    // without giving the engine a rendering path.
    for (i, stage) in cfg.pipeline.stages.iter().enumerate() {
        check(
            &format!("pipeline.stages[{i}].prompt"),
            &stage.prompt,
            STAGE_VARS,
            false,
        );
    }
}

/// `[serve]` invariants the schema walk cannot express. A wildcard CORS origin
/// is rejected: the control API is bearer-token authenticated, and `*` must
/// never be paired with credentialed cross-origin fetch.
fn check_serve(cfg: &Config, errs: &mut Vec<String>) {
    if let Err(error) = cfg.serve.resolve_transport(None, false) {
        errs.push(format!("serve transport: {error}"));
    }
    for origin in &cfg.serve.cors_origins {
        if origin.trim() == "*" {
            errs.push(
                "serve.cors_origins: wildcard `*` is not allowed — a bearer-token API must \
                 list explicit origins (e.g. \"https://gui.example.com\")"
                    .to_string(),
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
    value: &serde_json::Value,
    path: &str,
    errs: &mut Vec<String>,
    check_types: bool,
) {
    // `Schema::Bool` (e.g. `additionalProperties: false`) constrains shape,
    // which the wholesale `Config` parse already enforces — nothing enum-shaped
    // to check.
    if let Schema::Object(obj) = schema {
        walk_object(obj, root, value, path, errs, check_types);
    }
}

/// Walk one schema node in lockstep with the TOML value it describes.
fn walk_object(
    obj: &SchemaObject,
    root: &RootSchema,
    value: &serde_json::Value,
    path: &str,
    errs: &mut Vec<String>,
    check_types: bool,
) {
    // Resolve `$ref` through the root's definitions.
    if let Some(reference) = &obj.reference {
        if let Some(def) = root.definitions.get(ref_name(reference)) {
            walk_schema(def, root, value, path, errs, check_types);
        }
        return;
    }
    // schemars 0.8 wraps a `$ref` field in `allOf` whenever the field carries
    // ANY metadata (a `default`, a doc comment, …), so every `allOf` constraint
    // still applies. `anyOf` / `oneOf` are alternatives instead: validate each
    // branch in isolation and accept the union as soon as one branch is clean.
    if let Some(sub) = &obj.subschemas {
        for s in sub.all_of.iter().flatten() {
            walk_schema(s, root, value, path, errs, check_types);
        }
        if let Some(branches) = &sub.any_of {
            walk_union(branches, root, value, path, errs, check_types);
        }
        if let Some(branches) = &sub.one_of {
            walk_union(branches, root, value, path, errs, check_types);
        }
    }
    // `sandbox.failover` and `env.<name>.failover` retain a legacy boolean
    // spelling through their custom deserializers, although schemars exposes
    // the enum's canonical string shape. Preserve that compatibility while
    // still type-checking every other schema node.
    let legacy_failover_bool = value.is_boolean() && path.rsplit('.').next() == Some("failover");
    if check_types
        && !legacy_failover_bool
        && let Some(expected) = expected_type(obj, value)
        && !value_matches_type(value, &expected)
    {
        errs.push(format!(
            "{path}: expected {expected}, got {}",
            value_type(value)
        ));
        return;
    }
    crate::config_duration::check_number(obj, value, path, errs);
    // The strict enum check: only nodes carrying the `config_enum!` marker,
    // and only string TOML values — `failover` keys legally accept a bool
    // (`de_failover`), and genuinely wrong types are already reported by the
    // wholesale `Config` parse above.
    if let serde_json::Value::String(s) = value
        && let Some(marker) = obj.extensions.get(ENUM_MARKER)
        && let Err(e) = check_enum(obj, marker, s)
    {
        errs.push(format!("{path}: {e}"));
    }
    match value {
        serde_json::Value::Object(table) => {
            if let Some(ov) = &obj.object {
                for (key, child) in table {
                    let child_path = join_key(path, key);
                    if let Some(prop) = ov.properties.get(key) {
                        walk_schema(prop, root, child, &child_path, errs, check_types);
                    } else if let Some(additional) = &ov.additional_properties {
                        // Map tables: `[env.<name>]`, `[host.<name>]`, …
                        walk_schema(additional, root, child, &child_path, errs, check_types);
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
        serde_json::Value::Array(items) => {
            if let Some(av) = &obj.array
                && let Some(SingleOrVec::Single(item)) = &av.items
            {
                for (i, child) in items.iter().enumerate() {
                    walk_schema(
                        item,
                        root,
                        child,
                        &format!("{path}[{i}]"),
                        errs,
                        check_types,
                    );
                }
            }
        }
        _ => {}
    }
}

/// Validate a schema union without leaking errors from non-matching branches.
/// JSON Schema's `oneOf` normally also requires exactly one match, but the
/// config schemas use these lists only to describe serde alternatives; for
/// validation, either union is satisfied by any clean branch.
fn walk_union(
    branches: &[Schema],
    root: &RootSchema,
    value: &serde_json::Value,
    path: &str,
    errs: &mut Vec<String>,
    check_types: bool,
) {
    let matched = branches.iter().any(|branch| match branch {
        Schema::Bool(allowed) => *allowed,
        Schema::Object(_) => {
            let mut branch_errs = Vec::new();
            walk_schema(branch, root, value, path, &mut branch_errs, check_types);
            branch_errs.is_empty()
        }
    });
    if matched {
        return;
    }

    let mut shapes = Vec::new();
    for branch in branches {
        collect_schema_shapes(branch, root, &mut shapes);
    }
    shapes.sort();
    shapes.dedup();
    let accepted = if shapes.is_empty() {
        "a matching union branch".to_string()
    } else {
        shapes.join(" or ")
    };
    errs.push(format!(
        "{path}: expected one of: {accepted}, got {}",
        value_type(value)
    ));
}

/// Collect concise, user-facing shape names for a union diagnostic.
fn collect_schema_shapes(schema: &Schema, root: &RootSchema, out: &mut Vec<String>) {
    let Schema::Object(obj) = schema else {
        if matches!(schema, Schema::Bool(true)) {
            out.push("any value".to_string());
        }
        return;
    };
    if let Some(reference) = &obj.reference {
        if let Some(def) = root.definitions.get(ref_name(reference)) {
            collect_schema_shapes(def, root, out);
        }
        return;
    }
    if let Some(types) = &obj.instance_type {
        match types {
            SingleOrVec::Single(instance) => out.push(instance_type_name(instance)),
            SingleOrVec::Vec(instances) => {
                out.extend(instances.iter().map(instance_type_name));
            }
        }
        return;
    }
    if obj.object.is_some() {
        out.push("table/object".to_string());
        return;
    }
    if obj.array.is_some() {
        out.push("array".to_string());
        return;
    }
    if let Some(sub) = &obj.subschemas {
        for branch in sub
            .all_of
            .iter()
            .flatten()
            .chain(sub.any_of.iter().flatten())
            .chain(sub.one_of.iter().flatten())
        {
            collect_schema_shapes(branch, root, out);
        }
    }
}

fn expected_type(obj: &SchemaObject, value: &serde_json::Value) -> Option<String> {
    let types = obj.instance_type.as_ref()?;
    let mut names = match types {
        SingleOrVec::Single(t) => vec![instance_type_name(t)],
        SingleOrVec::Vec(ts) => ts.iter().map(instance_type_name).collect(),
    };
    if !value.is_null() && names.len() > 1 {
        names.retain(|name| name != "null");
    }
    Some(names.join(" or "))
}

fn instance_type_name(instance: &InstanceType) -> String {
    match instance {
        InstanceType::Null => "null",
        InstanceType::Boolean => "boolean",
        InstanceType::Object => "table/object",
        InstanceType::Array => "array",
        InstanceType::Number => "number",
        InstanceType::Integer => "integer",
        InstanceType::String => "string",
    }
    .to_string()
}

fn value_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "table/object",
    }
}

fn value_matches_type(value: &serde_json::Value, expected: &str) -> bool {
    expected.split(" or ").any(|kind| match kind {
        "number" => matches!(value, serde_json::Value::Number(_)),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "null" => value.is_null(),
        other => value_type(value) == other,
    })
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

    fn audit_cfg(backend: crate::config::SandboxBackend, chain: &[&str]) -> Config {
        let mut cfg = Config::default();
        cfg.sandbox.network_audit = true;
        cfg.sandbox.backend = backend;
        if !chain.is_empty() {
            cfg.sandbox.backend_chain = chain.iter().map(|name| name.to_string()).collect();
        }
        cfg
    }

    #[test]
    fn validate_warns_network_audit_on_docker() {
        let warnings =
            network_audit_warnings(&audit_cfg(crate::config::SandboxBackend::Docker, &[]));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("docker"), "{warnings:?}");
        assert!(
            warnings[0].contains("not a DNS-query or connection log"),
            "{warnings:?}"
        );
        // The same warning reaches `config validate`.
        let diagnostics =
            validate_diagnostics("[sandbox]\nbackend = \"docker\"\nnetwork_audit = true\n");
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == ValidationSeverity::Warning
                    && d.message.contains("network_audit")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn validate_silent_network_audit_on_podman() {
        for backend in [
            crate::config::SandboxBackend::Podman,
            crate::config::SandboxBackend::PodmanRootful,
        ] {
            assert!(network_audit_warnings(&audit_cfg(backend, &[])).is_empty());
            let diagnostics = validate_diagnostics(&format!(
                "[sandbox]\nbackend = \"{}\"\nnetwork_audit = true\n",
                backend.as_str()
            ));
            assert!(
                !diagnostics
                    .iter()
                    .any(|d| d.message.contains("network_audit")),
                "{diagnostics:?}"
            );
        }
        // The default `auto` chain starts with podman.
        assert!(
            network_audit_warnings(&audit_cfg(crate::config::SandboxBackend::Auto, &[])).is_empty()
        );
        let diagnostics = validate_diagnostics("[sandbox]\nnetwork_audit = true\n");
        assert!(
            !diagnostics
                .iter()
                .any(|d| d.message.contains("network_audit")),
            "{diagnostics:?}"
        );
        // Off: nothing to say, whatever the backend.
        let mut off = audit_cfg(crate::config::SandboxBackend::Docker, &[]);
        off.sandbox.network_audit = false;
        assert!(network_audit_warnings(&off).is_empty());
    }

    #[test]
    fn validate_warns_network_audit_auto_chain_without_podman() {
        let warnings = network_audit_warnings(&audit_cfg(
            crate::config::SandboxBackend::Auto,
            &["docker", "bwrap"],
        ));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("docker, bwrap"), "{warnings:?}");
        // The warning reaches `config validate` for an explicit non-podman chain.
        let diagnostics = validate_diagnostics(
            "[sandbox]\nbackend_chain = [\"docker\", \"bwrap\"]\nnetwork_audit = true\n",
        );
        assert!(
            diagnostics
                .iter()
                .any(|d| d.severity == ValidationSeverity::Warning
                    && d.message.contains("network_audit")),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn voice_credential_requires_tls_except_for_loopback_http() {
        for (body, accepted) in [
            (
                "[voice]\nkind = \"http\"\ncredential = \"env:TOKEN\"\nendpoint = \"http://127.0.0.2/stt\"\n",
                true,
            ),
            (
                "[voice]\nkind = \"http\"\ncredential = \"env:TOKEN\"\nendpoint = \"http://[::1]/stt\"\n",
                true,
            ),
            (
                "[voice]\nkind = \"http\"\ncredential = \"env:TOKEN\"\nendpoint = \"http://localhost/stt\"\n",
                true,
            ),
            (
                "[voice]\nkind = \"http\"\ncredential = \"env:TOKEN\"\nendpoint = \"https://stt.example/stt\"\n",
                true,
            ),
            (
                "[voice]\nkind = \"http\"\ncredential = \"env:TOKEN\"\nendpoint = \"http://stt.example/stt\"\n",
                false,
            ),
            (
                "[voice]\nkind = \"http\"\ncredential = \"env:TOKEN\"\nendpoint = \"not a URL\"\n",
                false,
            ),
            (
                "[voice]\nkind = \"http\"\nendpoint = \"http://stt.example/stt\"\n",
                true,
            ),
            (
                "[voice]\nkind = \"command\"\ncredential = \"env:TOKEN\"\nendpoint = \"http://stt.example/stt\"\n",
                true,
            ),
        ] {
            let errors = validate_str(body);
            assert_eq!(errors.is_empty(), accepted, "{body}: {errors:?}");
            if !accepted {
                assert!(errors[0].contains("use https"), "{errors:?}");
            }
        }
    }

    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    struct UnionDocument {
        value: StringOrObject,
    }

    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    #[serde(untagged)]
    enum StringOrObject {
        String(String),
        Object(UnionObject),
    }

    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    struct UnionObject {
        command: String,
    }

    #[allow(dead_code)]
    #[derive(schemars::JsonSchema)]
    struct StrictStringDocument {
        value: String,
    }

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
        // 73 → 74 (THE-16): `[mcp_servers.<name>.proxy] scope` (ProxyScope) —
        // the mcp-proxy hub's partition granularity.
        // 74 → 75: `[notifications.push] kind` (PushKind) — the push-to-phone
        // outbound delivery provider seam.
        // 75 → 76: `[[presets]] mode` (PresetMode) — the launch menu's named
        // launch shapes (split vs one-tab-per-command).
        // 76 → 77 (THE-14): `[drawer] kind` (DrawerKind) — the file-manager
        // provider seam (yazi/custom implemented; lf/broot reserved).
        // 77 → 78 (THE-5): `[search] structural` (StructuralKind) — the AST
        // search/rewrite tier for workspace Search & Replace.
        // 78 → 79 (THE-8): `[host_discovery] kind` (HostDiscoveryKind) — the
        // inbound host-discovery seam (`tailnet` implemented; `mdns`/`consul`
        // reserved).
        // 79 → 81 (THE-30): `[merge_queue] land_strategy` (LandStrategy) and
        // `[git] structural_diff` (StructuralDiff) — SCM workflow customization.
        // 81 → 83 (THE-47): `[sandbox] isolation_floor` (IsolationFloor) — the
        // minimum isolation class a sandbox may resolve to — and `[sandbox]
        // on_floor_miss` (OnFloorMiss) — what to do when the resolved backend
        // sits below that floor.
        // 83 → 86 (THE-58): `[model_proxy]` resurrection added ModelProviderKind
        // (provider wire protocol, implemented-or-reserved), RoutingStrategy
        // (sequential/load_balanced/cost_aware), and BudgetBreach
        // (warn/refuse/downgrade) — all strict-checked by construction.
        // 86 → 87: `[[metrics.targets]] kind` (MetricsTargetKind) — prometheus
        // scrape vs command collector.
        // 87 → 88: `[[pipeline.stages]] on_blocked` (OnBlocked) — what a
        // supervising agent does with a stage row that blocked or timed out.
        // Advisory vocabulary the Lead reads; thegn validates the spelling and
        // never takes the action itself.
        // 88 → 90 (THE-46): `[weather] provider` (WeatherProviderKind — `wttr_in`
        // implemented, `open_meteo`/`openweathermap` reserved) and `[weather] units`
        // (WeatherUnits).
        // 90 → 91 (THE-56): `[autopilot] open_as` (AutopilotOpenAs).
        // 90 → 91 (THE-11): `[[tools]] drawer_scope` (DrawerScope) — which
        // eligible catalog entries can occupy the bottom drawer.
        // 91 → 92 (THE-17): `[editor] provider` (EditorProvider) — the
        // logical external-editor handoff implementation.
        // 92 → 93: `[database] migration_authority` (MigrationAuthority) — which
        // process kind may advance the shared state schema. Added after an
        // unlanded branch's worker migrated the live database out from under
        // main and locked the supervisor's own CLI out of the roster.
        // 93 → 94 (THE-23): `[sandbox] devcontainer`
        // (DevcontainerMode) — repo-authored devcontainer overlay mode.
        // 94 → 95 (THE-59): `[voice] kind` (VoiceKind — generic command
        // provider).
        // 96 → 97 (THE-48): `[ci.autofix] mode` (CiAutofixMode). The provider
        // enum was already present on main.
        // 97 → 98 (THE-60): `[toolchain.mise] inject` (MiseInject). Both
        // branches independently claimed the 96 → 97 slot; merged, each enum is
        // still present, so the ledger records them in turn.
        // 98 → 99 (THE-32): `[git] submodules` (SubmoduleMode) — lifecycle
        // initialization policy, trusted config only.
        // 99 → 101 (THE-90, THE-101): `[sandbox] compiler_cache`
        // (SandboxCompilerCache) makes cache enablement explicit and trusted,
        // while `[serve] topology` (ServeTopology) distinguishes direct,
        // TLS-terminated, and tunnel-only control-plane deployments.
        // 101 → 102 (THE-2002): `[ui] sidebar_detail_when` (DetailWhen) —
        // whether the sidebar detail line needs sidebar focus.
        // 102 → 105 (THE-2021): `[sandbox.git]` mode (SandboxGitMode),
        // transport (GitTransport) and credential (GitCredential) — the
        // sandbox git write policy.
        // 105 → 106 (b139): `[ui] sidebar_external_worktrees`
        // (ExternalWorktrees) — group/show/hide for worktrees thegn did not
        // create.
        // 106 → 107 (THE-2220): `[lead] network` (LeadNetwork) — the Lead
        // sandbox's network namespace (isolated via pasta, or host).
        // 107 → 108 (THE-2233): `[env.<name>.provider] model_credentials`
        // (ModelCredentials) — the Vercel credential mode.
        // 108 → 109 (THE-2200): `[merge_queue] gate_sandbox` (GateSandbox) —
        // whether the merge gate runs on the host or in the sealed sandbox.
        // 109 → 110 (THE-2214): `[[inbox.accounts]] kind` (InboxKind —
        // command/todoist/imap implemented; gmail/jmap/outlook/slack reserved).
        assert_eq!(
            defs.len(),
            110,
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
        has("toolchain.mise.inject", "MiseInject");
        has("issues.provider", "IssueProviderKind");
        has("ui.sidebar_focus_detail", "FocusDetail");
        has("ui.sidebar_detail_when", "DetailWhen");
        has("sandbox.git.mode", "SandboxGitMode");
        has("sandbox.git.transport", "GitTransport");
        has("sandbox.git.credential", "GitCredential");
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
    fn union_accepts_each_shape_and_reports_only_the_union_on_a_miss() {
        assert!(
            validate_schema_value::<UnionDocument>(&serde_json::json!({ "value": "echo ok" }))
                .is_empty()
        );
        assert!(
            validate_schema_value::<UnionDocument>(
                &serde_json::json!({ "value": { "command": "echo ok" } })
            )
            .is_empty()
        );

        let errs = validate_schema_value::<UnionDocument>(
            &serde_json::json!({ "value": ["neither shape"] }),
        );
        assert_eq!(
            errs,
            ["value: expected one of: string or table/object, got array"]
        );

        // Exercise the `oneOf` field directly as schemars uses `anyOf` for
        // serde's untagged enums. Config validation intentionally gives both
        // union spellings the same any-clean-branch semantics.
        let string = SchemaObject {
            instance_type: Some(InstanceType::String.into()),
            ..Default::default()
        };
        let object = SchemaObject {
            instance_type: Some(InstanceType::Object.into()),
            ..Default::default()
        };
        let one_of = SchemaObject {
            subschemas: Some(Box::new(schemars::schema::SubschemaValidation {
                one_of: Some(vec![Schema::Object(string), Schema::Object(object)]),
                ..Default::default()
            })),
            ..Default::default()
        };
        let root = schemars::schema_for!(StrictStringDocument);
        let mut errs = Vec::new();
        walk_object(
            &one_of,
            &root,
            &serde_json::json!(false),
            "value",
            &mut errs,
            true,
        );
        assert_eq!(
            errs,
            ["value: expected one of: string or table/object, got boolean"]
        );
    }

    #[test]
    fn hooks_list_entries_accept_both_union_shapes() {
        let body = r#"
[hooks]
pre_create = [
  "echo shorthand",
  { command = "echo object", timeout_secs = 30, on_failure = "block" },
]
"#;
        assert!(validate_str(body).is_empty(), "{:#?}", validate_str(body));

        let errs = validate_str("[hooks]\npre_create = [42]\n");
        assert_eq!(errs.len(), 1, "{errs:#?}");
        assert!(errs[0].contains("hooks.pre_create[0]"), "{errs:#?}");
        assert!(
            errs[0].contains("expected one of: string or table/object, got integer"),
            "{errs:#?}"
        );
    }

    #[test]
    fn app_tabs_are_schema_listed_and_unknown_ids_are_actionable() {
        let schema = serde_json::to_value(schemars::schema_for!(Config)).unwrap();
        let apps = &schema["definitions"]["AppsConfig"];
        for id_schema in [
            &apps["properties"]["default_tab"],
            &apps["properties"]["tab_order"]["items"],
        ] {
            assert_eq!(id_schema["type"], "string");
            assert_eq!(
                id_schema["anyOf"][0]["enum"],
                serde_json::json!(["work", "observe"])
            );
            assert_eq!(
                id_schema["anyOf"][1]["pattern"],
                "^plugin:[A-Za-z0-9._-]+:[A-Za-z0-9._-]+$"
            );
        }
        assert!(validate_str("[apps]\ndefault_tab = \"plugin:todoist:workspace\"\ntab_order = [\"work\", \"plugin:todoist:workspace\"]\n").is_empty());
        for invalid in [
            "plugin:todoist",
            "plugin::workspace",
            "plugin:todoist:bad/name",
        ] {
            assert!(!validate_str(&format!("[apps]\ndefault_tab = {invalid:?}\n")).is_empty());
        }

        let errors = validate_str("[apps]\ntab_order = [\"work\", \"observe-2\"]\n");
        assert!(
            errors.iter().any(|error| {
                error.contains("observe-2")
                    && error
                        .contains("known tabs are work, observe or plugin:<plugin>:<contribution>")
            }),
            "{errors:#?}"
        );
    }

    #[test]
    fn app_default_must_be_enabled_and_duplicate_order_is_deduplicated() {
        let errors = validate_str("[apps]\ndefault_tab = \"observe\"\n");
        assert!(
            errors
                .iter()
                .any(|error| { error.contains("apps.default_tab") && error.contains("disabled") }),
            "{errors:#?}"
        );

        let cfg = toml::from_str::<Config>(
            "[observe]\nenabled = true\n[apps]\ndefault_tab = \"observe\"\ntab_order = [\"observe\", \"work\", \"observe\"]\n",
        )
        .unwrap();
        assert_eq!(
            cfg.apps.effective_tab_order(cfg.observe.enabled),
            ["observe", "work"]
        );
        assert!(validate_str(
            "[observe]\nenabled = true\n[apps]\ndefault_tab = \"observe\"\ntab_order = [\"observe\", \"work\", \"observe\"]\n"
        )
        .is_empty());

        // Mentioning a known but disabled app in the order is harmless; the
        // app's own enable flag controls membership.
        let disabled = toml::from_str::<Config>("[apps]\ntab_order = [\"observe\"]\n").unwrap();
        assert!(validate_str("[apps]\ntab_order = [\"observe\"]\n").is_empty());
        assert_eq!(
            disabled.apps.effective_tab_order(disabled.observe.enabled),
            ["work"]
        );
    }

    #[test]
    fn non_union_types_remain_strict() {
        let errs =
            validate_schema_value::<StrictStringDocument>(&serde_json::json!({ "value": false }));
        assert_eq!(errs, ["value: expected string, got boolean"]);
    }

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
    fn provider_static_grammar_is_part_of_strict_config_validation() {
        let cases = [
            (
                "[env.fly.provider]\nprovider = \"fly\"\nsize = \"shared-cpu-3x\"\n",
                "unknown Fly guest preset",
            ),
            (
                "[env.fly.provider]\nprovider = \"fly\"\ntemplate = \"snapshot:42\"\n",
                "Fly does not support snapshot image forms",
            ),
            (
                "[env.digitalocean.provider]\nprovider = \"digitalocean\"\ntemplate = \"snapshot:nope\"\n",
                "snapshot id is malformed",
            ),
            (
                "[env.hetzner.provider]\nprovider = \"hetzner\"\napi_base = \"https://user:token@example.invalid/v1\"\n",
                "endpoint host is missing or has credentials",
            ),
            (
                "[env.hetzner.provider]\nprovider = \"hetzner\"\nmax_instances = 257\n",
                "provider safety bound",
            ),
        ];
        for (body, reason) in cases {
            let errors = validate_str(body);
            assert!(
                errors.iter().any(|error| error.contains(reason)),
                "{body:?} -> {errors:?}"
            );
            assert!(
                !errors.iter().any(|error| error.contains("token")),
                "provider diagnostics must not echo endpoint credentials: {errors:?}"
            );
        }

        let valid = r#"
[env.fly.provider]
provider = "fly"
api_base = "http://127.0.0.1:8080/v1"
size = "shared-cpu-2x"
template = "image:registry.example/dev:1"
"#;
        assert!(validate_str(valid).is_empty(), "{:?}", validate_str(valid));
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
            ("[media]\nbackend = \"spotify\"\n", true),
            ("[media]\nbackend = \"jellyfin\"\n", true),
            ("[media]\nbackend = \"mpv\"\n", false),
            ("[sandbox]\nbackend = \"wsl\"\n", true),
            ("[sandbox]\nbackend = \"podman\"\n", false),
            ("[ci]\nprovider = \"argo\"\n", true),
            ("[ci]\nprovider = \"gitlab\"\n", false),
            ("[search]\nstructural = \"comby\"\n", true),
            ("[search]\nstructural = \"gritql\"\n", true),
            ("[search]\nstructural = \"ast-grep\"\n", false),
            ("[search]\nstructural = \"none\"\n", false),
            // A reserved kind lenient-loads as the default `command` kind, so
            // the fixture carries an argv: the reserved error must be the
            // only (and first) one.
            (
                "[[inbox.accounts]]\nname = \"m\"\nkind = \"gmail\"\ncommand = [\"feed\"]\n",
                true,
            ),
            (
                "[[inbox.accounts]]\nname = \"m\"\nkind = \"command\"\ncommand = [\"feed\"]\n",
                false,
            ),
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
    fn heavy_gate_rejects_negative_and_non_finite_load_limits() {
        let negative = validate_str("[sandbox.limits]\nmax_load = -1.0\n");
        assert!(
            negative
                .iter()
                .any(|error| error.contains("sandbox.limits.max_load")),
            "{negative:?}"
        );
        let mut cfg = Config::default();
        cfg.sandbox.limits.max_load = f64::NAN;
        let nan = typed_semantic_errors(&cfg, SemanticMode::AllDiagnostics);
        assert!(
            nan.iter()
                .any(|error| error.contains("sandbox.limits.max_load")),
            "{nan:?}"
        );
    }

    fn float_config(path: &str, value: f64) -> Config {
        let mut cfg = Config::default();
        match path {
            "media.volume_step" => cfg.media.volume_step = value,
            "usage.warn_percent" => cfg.usage.warn_percent = value as f32,
            "usage.crit_percent" => cfg.usage.crit_percent = value as f32,
            "activity.cpu_percent" => cfg.activity.cpu_percent = value,
            "activity.runaway_core_fraction" => cfg.activity.runaway_core_fraction = value,
            other => panic!("unknown float key {other}"),
        }
        cfg
    }

    #[test]
    #[should_panic(expected = "unknown float key")]
    fn float_config_rejects_an_unpinned_key() {
        float_config("media.unknown", 0.0);
    }

    fn list_config(toml_src: &str) -> Config {
        toml::from_str::<Config>(toml_src).expect("list config parses")
    }

    #[test]
    fn enum_value_warnings_flag_unknown_urgency_and_exit_policy() {
        assert!(enum_value_warnings(&Config::default()).is_empty());
        let mut cfg = Config::default();
        cfg.notifications.process_exit = "ALL".into();
        cfg.notifications.desktop_min_urgency = "CRITICAL".into();
        assert!(enum_value_warnings(&cfg).is_empty());
        cfg.notifications.process_exit = "sometimes".into();
        cfg.notifications.desktop_min_urgency = "urgent".into();
        let warnings = enum_value_warnings(&cfg);
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("notifications.process_exit"));
        assert!(warnings[1].contains("notifications.desktop_min_urgency"));
    }

    #[test]
    fn alert_order_warnings_flag_inverted_pairs() {
        assert!(alert_order_warnings(&Config::default()).is_empty());
        let mut cfg = Config::default();
        cfg.stats.alerts.cpu.warn = 90.0;
        cfg.stats.alerts.cpu.critical = 80.0;
        let warnings = alert_order_warnings(&cfg);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("stats.alerts.cpu:"));
        let mut cfg = Config::default();
        cfg.stats.alerts.battery.warn = 10.0;
        cfg.stats.alerts.battery.critical = 20.0;
        let warnings = alert_order_warnings(&cfg);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("stats.alerts.battery:"));
        let mut cfg = Config::default();
        cfg.stats.alerts.mem.warn = 70.0;
        cfg.stats.alerts.mem.critical = 70.0;
        assert_eq!(alert_order_warnings(&cfg).len(), 1);
        let mut cfg = Config::default();
        cfg.stats.alerts.cpu.warn = 0.0;
        cfg.stats.alerts.cpu.critical = 80.0;
        cfg.stats.alerts.battery.warn = 10.0;
        cfg.stats.alerts.battery.critical = 0.0;
        assert!(alert_order_warnings(&cfg).is_empty());
    }

    #[test]
    fn notification_value_warnings_table() {
        assert!(notification_value_warnings(&Config::default()).is_empty());
        let cases: [(&str, usize, &str); 5] = [
            ("[notifications.priority]\nagent_done = \"alert\"\n", 0, ""),
            (
                "[notifications.priority]\nbogus_kind = \"alert\"\n",
                1,
                "bogus_kind",
            ),
            (
                "[notifications.priority]\nagent_done = \"loud\"\n",
                1,
                "loud",
            ),
            (
                "[notifications]\nactive_mode = \"focus\"\n",
                1,
                "active_mode",
            ),
            (
                "[notifications]\nagent_error_signatures = [\"\", \"boom\"]\n",
                1,
                "agent_error_signatures",
            ),
        ];
        for (body, count, needle) in cases {
            let cfg: Config = toml::from_str(body).expect("parse pretty");
            let w = notification_value_warnings(&cfg);
            assert_eq!(w.len(), count, "{body}: {w:?}");
            assert!(w.iter().all(|m| m.contains(needle)), "{w:?}");
            // Minified form parses to the same result.
            let mini = body.replace(" = ", "=").replace(", ", ",");
            let cfg: Config = toml::from_str(&mini).expect("parse minified");
            assert_eq!(notification_value_warnings(&cfg).len(), count, "{mini}");
        }
    }

    #[test]
    fn notification_value_warnings_reach_diagnostics_as_warnings() {
        let body = "[notifications]\nactive_mode = \"focus\"\n[notifications.priority]\nnope = \"alert\"\n";
        let diagnostics = validate_diagnostics(body);
        assert!(diagnostics.iter().any(
            |d| d.severity == ValidationSeverity::Warning && d.message.contains("active_mode")
        ));
        assert!(
            diagnostics
                .iter()
                .all(|d| d.severity != ValidationSeverity::Error)
        );
    }

    #[test]
    fn enum_and_alert_warnings_are_ignorable_diagnostics() {
        let body = "[notifications]\nprocess_exit = \"sometimes\"\n[stats.alerts.cpu]\nwarn = 90.0\ncritical = 80.0\n";
        let diagnostics = validate_diagnostics(body);
        let warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == ValidationSeverity::Warning)
            .collect();
        assert!(warnings.iter().any(|d| d.message.contains("process_exit")));
        assert!(
            warnings
                .iter()
                .any(|d| d.message.contains("stats.alerts.cpu"))
        );
        assert!(
            diagnostics
                .iter()
                .all(|d| d.severity != ValidationSeverity::Error)
        );
    }

    #[test]
    fn list_value_warnings_flag_bad_dnd_windows_and_unknown_providers() {
        assert!(list_value_warnings(&Config::default()).is_empty());
        let good = list_config(
            "[notifications.dnd]\nwindows = [\"22:00-07:00\", \"mon-fri 09:00-17:00\", \"sat,sun 00:00-08:00\"]\n\
             [usage]\nproviders = [\"codex\", \"claude\", \"antigravity\"]\n",
        );
        assert!(list_value_warnings(&good).is_empty());
        let empty = list_config("[notifications.dnd]\nwindows = []\n[usage]\nproviders = []\n");
        assert!(list_value_warnings(&empty).is_empty());
        for bad in ["25:00-01:00", "nonsense", ""] {
            let cfg = list_config(&format!(
                "[notifications.dnd]\nwindows = [\"22:00-07:00\", {bad:?}]\n"
            ));
            let warnings = list_value_warnings(&cfg);
            assert_eq!(warnings.len(), 1, "{bad:?}: {warnings:?}");
            assert!(
                warnings[0].contains("notifications.dnd.windows[1]"),
                "{warnings:?}"
            );
        }
        let cfg = list_config("[usage]\nproviders = [\"codex\", \"frobnicate\"]\n");
        let warnings = list_value_warnings(&cfg);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("usage.providers[1]"), "{warnings:?}");
        assert!(warnings[0].contains("frobnicate"), "{warnings:?}");
    }

    #[test]
    fn bad_dnd_window_is_an_ignorable_warning_not_an_error() {
        let diagnostics = validate_diagnostics("[notifications.dnd]\nwindows = [\"nonsense\"]\n");
        let hit = diagnostics
            .iter()
            .find(|d| d.message.contains("notifications.dnd.windows[0]"))
            .expect("bad window diagnostic");
        assert_eq!(hit.severity, ValidationSeverity::Warning);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.severity != ValidationSeverity::Error),
            "{diagnostics:?}"
        );
    }

    #[test]
    fn float_key_ranges_warn_but_never_error_on_out_of_range_and_non_finite_values() {
        assert!(float_range_warnings(&Config::default()).is_empty());
        for (path, min, max) in FLOAT_KEY_RANGES {
            for accepted in [min, max, (min + max) / 2.0] {
                let cfg = float_config(path, accepted);
                assert!(float_range_warnings(&cfg).is_empty(), "{path} = {accepted}");
            }
            for out_of_range in [
                -1.0,
                min - 0.001,
                max + 0.001,
                f64::NAN,
                f64::INFINITY,
                f64::NEG_INFINITY,
            ] {
                let cfg = float_config(path, out_of_range);
                // Never an admission error: startup must not be refused.
                assert!(
                    typed_semantic_errors(&cfg, SemanticMode::AllDiagnostics)
                        .iter()
                        .all(|error| !error.contains(path)),
                    "{path} = {out_of_range} must not be an error"
                );
                let warnings = float_range_warnings(&cfg);
                assert_eq!(warnings.len(), 1, "{path} = {out_of_range}: {warnings:?}");
                assert!(
                    warnings[0].starts_with(&format!("{path} = ")),
                    "{warnings:?}"
                );
                assert!(
                    warnings[0].contains(&format!("{min}..={max}")),
                    "{warnings:?}"
                );
            }
        }
        let cfg = float_config("usage.warn_percent", 101.0);
        assert_eq!(
            float_range_warnings(&cfg),
            vec![
                "usage.warn_percent = 101 is outside the valid range 0..=100 (finite numbers only); the value is kept"
                    .to_string()
            ]
        );
    }

    fn refresh_rates_config(rates: &[f64]) -> Config {
        let mut cfg = Config::default();
        cfg.stats.refresh_rates = rates.to_vec();
        cfg
    }

    fn refresh_rate_warnings(rates: &[f64]) -> Vec<String> {
        let cfg = refresh_rates_config(rates);
        // Never an admission error: startup must not be refused.
        assert!(
            typed_semantic_errors(&cfg, SemanticMode::AllDiagnostics)
                .iter()
                .all(|error| !error.contains("stats.refresh_rates")),
            "{rates:?} must not be an error"
        );
        float_range_warnings(&cfg)
    }

    #[test]
    fn refresh_rates_accept_the_default_empty_and_endpoint_lists() {
        assert_eq!(REFRESH_RATE_RANGE, (0.5, 315_360_000.0));
        assert!(
            Config::default()
                .stats
                .refresh_rates
                .iter()
                .all(|rate| *rate >= REFRESH_RATE_RANGE.0)
        );
        assert!(refresh_rate_warnings(&Config::default().stats.refresh_rates).is_empty());
        assert!(refresh_rate_warnings(&[]).is_empty());
        assert!(refresh_rate_warnings(&[0.5, 315_360_000.0]).is_empty());
    }

    #[test]
    fn refresh_rates_warn_once_per_bad_entry_and_never_error() {
        assert_eq!(
            refresh_rate_warnings(&[0.1]),
            vec![
                "stats.refresh_rates[0] = 0.1 is outside the valid range 0.5..=315360000 (finite numbers only); the value is kept"
                    .to_string()
            ]
        );
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 315_360_000.5] {
            let warnings = refresh_rate_warnings(&[bad]);
            assert_eq!(warnings.len(), 1, "{bad}: {warnings:?}");
            assert!(
                warnings[0].starts_with(&format!("stats.refresh_rates[0] = {bad} ")),
                "{warnings:?}"
            );
            assert!(warnings[0].contains("0.5..=315360000"), "{warnings:?}");
        }
        assert_eq!(
            refresh_rate_warnings(&[1.0, -2.0, 3.0]),
            vec![
                "stats.refresh_rates[1] = -2 is outside the valid range 0.5..=315360000 (finite numbers only); the value is kept"
                    .to_string()
            ]
        );
        let warnings = refresh_rate_warnings(&[0.0, 2.0, f64::NAN, 1e12]);
        assert_eq!(
            warnings
                .iter()
                .map(|warning| warning.split(" = ").next().unwrap_or_default())
                .collect::<Vec<&str>>(),
            vec![
                "stats.refresh_rates[0]",
                "stats.refresh_rates[2]",
                "stats.refresh_rates[3]",
            ]
        );
    }

    #[test]
    fn refresh_rate_warnings_reach_validate_diagnostics_as_warnings() {
        let body = "[stats]\nrefresh_rates = [1.0, 0.25, 2.0]\n";
        assert!(validate_str(body).is_empty(), "{:?}", validate_str(body));
        let warnings = float_warnings_in(body, "stats.refresh_rates");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].starts_with("stats.refresh_rates[1] = 0.25 "),
            "{warnings:?}"
        );
        assert!(validate_diagnostics("[stats]\nrefresh_rates = []\n").is_empty());
    }

    #[test]
    fn sprites_model_credentials_explicitness_reaches_validate_diagnostics() {
        let explicit = "[env.s]\nplacement = \"provider\"\n[env.s.provider]\nprovider = \"sprites\"\nmodel_credentials = \"none\"\n";
        let implicit =
            "[env.s]\nplacement = \"provider\"\n[env.s.provider]\nprovider = \"sprites\"\n";
        assert!(
            !validate_diagnostics(explicit)
                .iter()
                .any(|diagnostic| diagnostic.message.contains("forwarded by default"))
        );
        assert_eq!(
            validate_diagnostics(implicit)
                .iter()
                .filter(|diagnostic| diagnostic.message.contains("forwarded by default"))
                .count(),
            1
        );
    }

    #[test]
    fn activity_float_ranges_warn_once_and_never_error() {
        let defaults = Config::default();
        assert!(float_range_warnings(&defaults).is_empty());
        assert!(
            FLOAT_KEY_RANGES
                .iter()
                .any(|(path, _, _)| *path == "activity.cpu_percent")
        );
        for (path, max) in [
            ("activity.cpu_percent", 100.0),
            ("activity.runaway_core_fraction", 1.0),
        ] {
            for accepted in [0.0, max] {
                assert!(
                    float_range_warnings(&float_config(path, accepted)).is_empty(),
                    "{path} = {accepted}"
                );
            }
            for rejected in [-0.5, max + 1.0, f64::NAN, f64::INFINITY] {
                let cfg = float_config(path, rejected);
                assert!(
                    typed_semantic_errors(&cfg, SemanticMode::AllDiagnostics)
                        .iter()
                        .all(|error| !error.contains(path)),
                    "{path} = {rejected} must not be an error"
                );
                let warnings = float_range_warnings(&cfg);
                assert_eq!(warnings.len(), 1, "{path} = {rejected}: {warnings:?}");
                assert!(warnings[0].starts_with(&format!("{path} = ")));
                assert!(warnings[0].contains(&format!("0..={max}")));
            }
        }
        assert_eq!(
            float_range_warnings(&float_config("activity.runaway_core_fraction", 1.5)),
            vec![
                "activity.runaway_core_fraction = 1.5 is outside the valid range 0..=1 (finite numbers only); the value is kept"
                    .to_string()
            ]
        );
    }

    #[test]
    fn activity_float_ranges_reach_validate_diagnostics_as_warnings() {
        let body = "[activity]\ncpu_percent = 150.0\nrunaway_core_fraction = -0.1\n";
        assert!(validate_str(body).is_empty(), "{:?}", validate_str(body));
        assert_eq!(float_warnings_in(body, "activity.cpu_percent").len(), 1);
        assert_eq!(
            float_warnings_in(body, "activity.runaway_core_fraction").len(),
            1
        );
        assert!(
            validate_diagnostics("[activity]\ncpu_percent = 100.0\nrunaway_core_fraction = 0.0\n")
                .is_empty()
        );
    }

    fn float_warnings_in(body: &str, path: &str) -> Vec<String> {
        validate_diagnostics(body)
            .into_iter()
            .filter(|d| d.severity == ValidationSeverity::Warning && d.message.contains(path))
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn sandbox_git_policy_warnings_cover_global_and_env_overlays() {
        let body = "[sandbox.git]\npush_refs = [\"refs/heads/m*\"]\n\n[env.box.sandbox.git]\ndeny_refs = [\"heads/main\"]\n";
        let warnings = float_warnings_in(body, "sandbox.git.");
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("sandbox.git.push_refs:")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("env.box.sandbox.git.deny_refs:")),
            "{warnings:?}"
        );
        // The overlay does not repeat the global table's own warning.
        assert!(
            !warnings
                .iter()
                .any(|w| w.starts_with("env.box.sandbox.git.push_refs:")),
            "{warnings:?}"
        );
        assert!(
            float_warnings_in("[sandbox.git]\nmode = \"commit-only\"\n", "sandbox.git.").is_empty()
        );
    }

    #[test]
    fn float_key_ranges_reach_validate_diagnostics_as_warnings_and_allow_crit_below_warn() {
        let body = "[media]\nvolume_step = 1.5\n";
        assert!(validate_str(body).is_empty(), "{:?}", validate_str(body));
        assert_eq!(float_warnings_in(body, "media.volume_step").len(), 1);
        let body = "[usage]\nwarn_percent = 101.0\ncrit_percent = -2.0\n";
        assert!(validate_str(body).is_empty(), "{:?}", validate_str(body));
        assert_eq!(float_warnings_in(body, "usage.warn_percent").len(), 1);
        assert_eq!(float_warnings_in(body, "usage.crit_percent").len(), 1);
        // crit below warn keeps the existing behaviour (treated as warn).
        assert!(
            validate_diagnostics("[usage]\nwarn_percent = 90.0\ncrit_percent = 50.0\n").is_empty()
        );
        assert!(validate_diagnostics("[media]\nvolume_step = 0.0\n").is_empty());
        assert!(
            validate_diagnostics("[usage]\nwarn_percent = 100.0\ncrit_percent = 0.0\n").is_empty()
        );
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

    #[test]
    fn legacy_warm_direnv_is_a_warning_but_never_an_error() {
        let diagnostics = validate_diagnostics("[sandbox]\nwarm_direnv = \"auto\"\n");
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].severity, ValidationSeverity::Warning);
        assert!(diagnostics[0].message.contains("deprecated and disabled"));
        assert!(validate_str("[sandbox]\nwarm_direnv = \"auto\"\n").is_empty());
    }

    #[test]
    fn unenforced_lifetime_is_a_warning_for_unreaped_providers() {
        for provider in ["machine0", "sprites"] {
            let body = format!(
                "[env.m0.provider]\nprovider = \"{provider}\"\nregion = \"us-east\"\nmax_lifetime_secs = 3600\n"
            );
            let matching: Vec<_> = validate_diagnostics(&body)
                .into_iter()
                .filter(|diagnostic| {
                    diagnostic
                        .message
                        .contains("env.m0.provider.max_lifetime_secs")
                        && diagnostic.message.contains("not enforced")
                })
                .collect();
            assert_eq!(matching.len(), 1, "{provider}: {matching:?}");
            assert_eq!(matching[0].severity, ValidationSeverity::Warning);
            assert!(validate_str(&body).is_empty(), "{provider}");
        }

        for provider in ["fly", "hetzner", "digitalocean"] {
            let body = format!(
                "[env.{provider}.provider]\nprovider = \"{provider}\"\nmax_lifetime_secs = 3600\n"
            );
            assert!(validate_diagnostics(&body).iter().all(|diagnostic| {
                !diagnostic
                    .message
                    .contains(&format!("env.{provider}.provider.max_lifetime_secs"))
            }));
        }

        let machine0_off = "[env.m0.provider]\nprovider = \"machine0\"\nmax_lifetime_secs = 0\n";
        assert!(validate_diagnostics(machine0_off).iter().all(|diagnostic| {
            !diagnostic
                .message
                .contains("env.m0.provider.max_lifetime_secs")
        }));
    }

    #[test]
    fn legacy_project_config_keys_are_known_compatibility_diagnostics() {
        let body = r#"workspaces_dir = "/legacy"
[ui]
confirm_delete_workspace = false
sidebar_workspace_sort = "attention"
[workspace.repo]
"#;
        assert!(validate_str(body).is_empty());
        let diagnostics = validate_diagnostics(body);
        assert_eq!(diagnostics.len(), 4);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.severity == ValidationSeverity::Warning)
        );
        let errs: Vec<_> = diagnostics.into_iter().map(|d| d.message).collect();
        assert!(!errs.iter().any(|e| e.contains("unknown key")), "{errs:?}");
        assert!(
            errs.iter()
                .any(|e| e.contains("workspaces_dir") && e.contains("projects_dir"))
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("workspace.repo") && e.contains("project.repo"))
        );
    }

    #[test]
    fn compatibility_warning_does_not_hide_real_errors_or_change_collision_policy() {
        let body = "projects_dir = '/canonical'\nworkspaces_dir = '/legacy'\npicker = 'invalid'\n";
        let diagnostics = validate_diagnostics(body);
        assert_eq!(
            diagnostics
                .iter()
                .filter(|d| d.severity == ValidationSeverity::Warning)
                .count(),
            1
        );
        let errors = validate_str(body);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("picker"));
        let warnings: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.severity == ValidationSeverity::Warning)
            .collect();
        assert!(warnings[0].message.contains("using canonical"));
        assert!(
            validate_str("projects_dir = '/canonical'\nworkspaces_dir = '/legacy'\n").is_empty()
        );
        let syntax = validate_diagnostics("x = =");
        assert_eq!(syntax.len(), 1);
        assert_eq!(syntax[0].severity, ValidationSeverity::Error);
    }

    #[test]
    fn malformed_sandbox_mount_is_a_warning() {
        let body = "[sandbox]\nmounts = [\"/a:/b:bogus\"]\n";
        let diagnostics = validate_diagnostics(body);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].severity, ValidationSeverity::Warning);
        assert!(diagnostics[0].message.contains("mounts[0]"));
        assert!(diagnostics[0].message.contains("bogus"));
        assert!(validate_str(body).is_empty());
    }

    #[test]
    fn warns_on_unknown_detail_field_and_badge_without_n() {
        let body = "[ui]\nsidebar_detail_fields = [\"branch\", \"bogus\"]\nsidebar_commit_badge = \"commits\"\nsidebar_behind_badge = \"{n}b\"\n";
        let diagnostics = validate_diagnostics(body);
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == ValidationSeverity::Warning
                && diagnostic.message.contains("sidebar_detail_fields")
                && diagnostic.message.contains("bogus")
                && diagnostic.message.contains("plugin:<name>/<field>")
        }));
        assert!(diagnostics.iter().any(|diagnostic| {
            diagnostic.severity == ValidationSeverity::Warning
                && diagnostic.message.contains("sidebar_commit_badge")
                && diagnostic.message.contains("{n}")
        }));
        assert!(validate_str(body).is_empty());
    }

    #[test]
    fn malformed_workspace_sandbox_mount_is_a_warning() {
        let body = "[project.repo]\nsandbox_mounts = [\"/a:relative\"]\n";
        let diagnostics = validate_diagnostics(body);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        assert_eq!(diagnostics[0].severity, ValidationSeverity::Warning);
        assert!(
            diagnostics[0]
                .message
                .contains("project.repo.sandbox_mounts[0]")
        );
        assert!(diagnostics[0].message.contains("relative"));
        assert!(validate_str(body).is_empty());
    }

    fn exposure_for(body: &str) -> Vec<String> {
        let cfg: Config = toml::from_str(body).unwrap();
        mount_exposure_warnings(
            &cfg,
            Some(std::path::Path::new("/run/user/1000")),
            std::path::Path::new("/h"),
        )
    }

    #[test]
    fn env_mount_of_run_user_warns_about_runtime_dir_exposure() {
        let warnings = exposure_for("[env.pipeline.sandbox]\nmounts = [\"/run/user\"]\n");
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("env.pipeline.sandbox.mounts[0]"));
        assert!(warnings[0].contains("exposes $XDG_RUNTIME_DIR"));
    }

    #[test]
    fn mount_exposure_warnings_label_every_mount_layer() {
        let body = "[sandbox]\nmounts = [\"/srv\", \"/run/user/1000\"]\n\
                    [profiles.work.sandbox]\nmounts = [\"/h/.claude-profiles\"]\n\
                    [project.repo]\nsandbox_mounts = [\"/run:ro\"]\n";
        let warnings = exposure_for(body);
        assert_eq!(warnings.len(), 3, "{warnings:?}");
        assert!(warnings[0].contains("sandbox.mounts[1]"));
        assert!(warnings[1].contains("profiles.work.sandbox.mounts[0]"));
        assert!(warnings[1].contains("~/.claude-profiles"));
        assert!(warnings[2].contains("project.repo.sandbox_mounts[0]"));
        assert!(exposure_for("[sandbox]\nmounts = [\"/run/user/1000/gnupg\"]\n").is_empty());
    }

    #[test]
    fn rw_suffix_mount_is_valid() {
        assert!(validate_diagnostics("[sandbox]\nmounts = [\"/a:/b:rw\"]\n").is_empty());
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

    #[test]
    fn serve_cors_wildcard_is_rejected_but_explicit_origins_pass() {
        let wildcard = validate_str("[serve]\ncors_origins = [\"*\"]\n");
        assert!(
            wildcard
                .iter()
                .any(|e| e.contains("cors_origins") && e.contains("wildcard")),
            "{wildcard:#?}"
        );
        // An explicit origin list validates clean.
        let ok = validate_str("[serve]\ncors_origins = [\"https://gui.example.com\"]\n");
        assert!(ok.is_empty(), "{ok:#?}");
        // Empty (the default) is clean.
        assert!(validate_str("[serve]\ncors_origins = []\n").is_empty());
    }

    #[test]
    fn serve_transport_rejects_accidental_plaintext_widening() {
        let widened = validate_str("[serve]\nbind = \"0.0.0.0:5380\"\n");
        assert!(
            widened.iter().any(|error| {
                error.contains("plaintext non-loopback")
                    && error.contains("unsafe_allow_plaintext_non_loopback")
            }),
            "{widened:#?}"
        );
        let opted_in = validate_str(
            "[serve]\nbind = \"0.0.0.0:5380\"\nadvertise_host = \"host.example\"\nunsafe_allow_plaintext_non_loopback = true\n",
        );
        assert!(opted_in.is_empty(), "{opted_in:#?}");
    }

    #[test]
    fn serve_transport_validates_strict_termination_boundary() {
        let missing = validate_str("[serve]\ntopology = \"tls-terminated\"\n");
        assert!(
            missing.iter().any(|error| error.contains("advertise_host")),
            "{missing:#?}"
        );
        let exposed_backend = validate_str(
            "[serve]\ntopology = \"tls-terminated\"\nbind = \"0.0.0.0:5380\"\nadvertise_host = \"host.example\"\n",
        );
        assert!(
            exposed_backend
                .iter()
                .any(|error| error.contains("loopback backend")),
            "{exposed_backend:#?}"
        );
        let valid = validate_str(
            "[serve]\ntopology = \"tls-terminated\"\nbind = \"127.0.0.1:5380\"\nadvertise_host = \"host.example\"\n",
        );
        assert!(valid.is_empty(), "{valid:#?}");
    }

    /// `[[pipeline.stages]]` reaches `validate_str` through all three channels:
    /// the schema walk (`on_blocked` spelling), the semantic pass
    /// (`validate_pipeline`), and the template pass (`STAGE_VARS`).
    #[test]
    fn pipeline_stages_are_validated_through_every_channel() {
        let body = r#"
[[agents]]
name = "worker"
command = "worker --run"

[[pipeline.stages]]
name = "architect"
agent = "worker"
prompt = "Row {row}: chunk {issue_title} into {artifact}, then `thegn dispatch report {row}`"
next = "code"

[[pipeline.stages]]
name = "code"
agent = "worker"
prompt = "Row {row}: implement {parent_artifact} for {stage}, then `thegn dispatch report {row}`"
concurrency = 3
on_blocked = "escalate"
"#;
        assert!(validate_str(body).is_empty(), "{:#?}", validate_str(body));

        // Completion policy is generated at runtime, so task text does not
        // need to mention the report command or the row placeholder.
        let no_contract = body.replace(", then `thegn dispatch report {row}`", "");
        assert!(validate_str(&no_contract).is_empty());
        // Dropping `{row}` is also valid because the renderer binds the row.
        let no_row = body.replace("Row {row}: ", "").replace(" {row}", " <id>");
        assert!(validate_str(&no_row).is_empty());

        // Schema walk: an unknown `on_blocked` spelling.
        let errs = validate_str(&body.replace("\"escalate\"", "\"retry\""));
        assert!(
            errs.iter()
                .any(|e| e.contains("on_blocked") && e.contains("retry")),
            "{errs:#?}"
        );

        // Semantic pass: an agent that names nothing launchable.
        let errs = validate_str(&body.replace("agent = \"worker\"", "agent = \"just dev\""));
        assert!(
            errs.iter()
                .any(|e| e.contains("pipeline.stages[0]") && e.contains(".agent:")),
            "{errs:#?}"
        );

        // Semantic pass: a `next` naming no stage.
        let errs = validate_str(&body.replace("next = \"code\"", "next = \"nope\""));
        assert!(
            errs.iter().any(
                |e| e.contains("pipeline.stages[0]") && e.contains("names no configured stage")
            ),
            "{errs:#?}"
        );

        // Template pass: a `{typo}` in a stage prompt is an error, not a silent
        // empty expansion at dispatch time.
        let errs = validate_str(&body.replace("{artifact}", "{artifcat}"));
        assert!(
            errs.iter()
                .any(|e| e.contains("pipeline.stages[0].prompt") && e.contains("artifcat")),
            "{errs:#?}"
        );
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

#[cfg(test)]
mod keybind_schema_tests {
    #[test]
    fn flat_and_mode_chords_preserve_strict_types() {
        for value in [
            serde_json::json!({"custom-action": "ctrl-x", "vim_normal": {"other": "x"}}),
            serde_json::json!({"emacs": {}, "vim_insert": {"other": "y"}}),
        ] {
            assert!(
                super::validate_schema_value::<crate::config::KeybindConfig>(&value).is_empty()
            );
        }
        for value in [
            serde_json::json!({"custom-action": 42}),
            serde_json::json!({"vim_normal": "ctrl-x"}),
            serde_json::json!({"vim_insert": {"other": false}}),
        ] {
            assert!(
                !super::validate_schema_value::<crate::config::KeybindConfig>(&value).is_empty()
            );
        }
    }
}

#[cfg(test)]
mod security_root_rot_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Field names that mean "this subtree can execute a process, hold or
    /// select a credential, or reach a remote". The vocabulary is drawn from
    /// the names THIS schema actually uses for those things — `run`, `args`,
    /// `*_bin`, bare `path`, `*_image`, `socket`, `proxy`, `*_dir` — because
    /// a root whose exec field is spelled `run` is exactly the one a
    /// command/argv-only vocabulary would miss.
    fn is_dangerous_field(name: &str) -> bool {
        // Names the suffix vocabulary matches by accident, each a
        // presentation knob that executes, holds and reaches nothing:
        // `[ui] sidebar_show_behind_base` is a boolean badge toggle ("behind
        // the base branch"), not a `*_base` URL.
        const NOT_DANGEROUS: &[&str] = &["sidebar_show_behind_base"];
        if NOT_DANGEROUS.contains(&name) {
            return false;
        }
        const EXACT: &[&str] = &[
            "command", "argv", "exec", "cmd", "run", "script", "shell", "args", "bin", "path",
            "image", "socket", "proxy", "url", "host", "endpoint", "server", "token", "secret",
            "api_key", "password", "key_path", "account", "accounts", "identity",
        ];
        const SUFFIX: &[&str] = &[
            "_command",
            "_argv",
            "_exec",
            "_args",
            "_bin",
            "_image",
            "_url",
            "_host",
            "_endpoint",
            "_token",
            "_secret",
            "_key",
            "_base",
            "_path",
            "_dir",
        ];
        EXACT.contains(&name) || SUFFIX.iter().any(|suffix| name.ends_with(suffix))
    }

    fn collect(root: &RootSchema, schema: &Schema, seen: &mut BTreeSet<String>) -> bool {
        let Schema::Object(obj) = schema else {
            return false;
        };
        if let Some(reference) = &obj.reference {
            let name = ref_name(reference).to_string();
            // Definitions recurse (plugins, providers); visit each once.
            if !seen.insert(name.clone()) {
                return false;
            }
            let found = root
                .definitions
                .get(&name)
                .is_some_and(|def| collect(root, def, seen));
            seen.remove(&name);
            return found;
        }
        let mut found = false;
        if let Some(sub) = &obj.subschemas {
            for branch in sub
                .all_of
                .iter()
                .chain(sub.any_of.iter())
                .chain(sub.one_of.iter())
                .flatten()
            {
                found |= collect(root, branch, seen);
            }
        }
        if let Some(ov) = &obj.object {
            for (name, child) in &ov.properties {
                if is_dangerous_field(name) {
                    found = true;
                }
                found |= collect(root, child, seen);
            }
            if let Some(additional) = &ov.additional_properties {
                found |= collect(root, additional, seen);
            }
        }
        if let Some(av) = &obj.array
            && let Some(SingleOrVec::Single(item)) = &av.items
        {
            found |= collect(root, item, seen);
        }
        found
    }

    /// The denylist rots as tables are added, and the relaxation is only safe
    /// while it is complete. Derive the expectation from the generated schema:
    /// any root whose subtree carries a command/argv/run, a credential, or a
    /// URL/host field must be listed. (A denylist — not an allowlist — is
    /// deliberate: a table a NEWER build adds must not brick an older one.)
    ///
    /// This is a LOWER BOUND, not a proof of exhaustiveness: it catches only
    /// what the field vocabulary above names, so roots whose danger is
    /// structural rather than lexical (`[database]`'s migration authority,
    /// `[placement]`'s lanes, `[disk]`'s reclamation) are listed on human
    /// judgment and are not re-derived here. The guard flags a majority of the
    /// listed roots; the assertions below fail if that detection collapses, so
    /// a schemars upgrade cannot quietly turn this test into decoration. The
    /// floor is derived from the list rather than written as a literal — a
    /// number in a comment about rot is itself the thing that rots.
    #[test]
    fn security_relevant_roots_cover_every_executing_or_credential_table() {
        let root = config_schema();
        let properties = &root
            .schema
            .object
            .as_ref()
            .expect("Config is an object schema")
            .properties;
        let mut missing = Vec::new();
        let mut flagged = BTreeSet::new();
        for (name, schema) in properties {
            let mut seen = BTreeSet::new();
            if is_dangerous_field(name) || collect(root, schema, &mut seen) {
                flagged.insert(name.clone());
                if !SECURITY_RELEVANT_ROOTS.contains(&name.as_str()) {
                    missing.push(name.clone());
                }
            }
        }
        assert!(
            missing.is_empty(),
            "these roots execute, hold credentials or reach a remote but are \
             not in SECURITY_RELEVANT_ROOTS (add them, and mirror the list in \
             docs/help/configuration.md + docs/ARCHITECTURE.md): {missing:?}"
        );
        // Non-vacuous: the walk must still SEE the dangerous subtrees. Without
        // this the test would pass if `collect` silently stopped matching.
        for expected in ["sandbox", "project", "agents", "env", "host", "metrics"] {
            assert!(
                flagged.contains(expected),
                "the schema walk no longer detects `{expected}`: the guard has \
                 become decoration (schemars shape change?). flagged={flagged:?}"
            );
        }
        // Half the list, derived: comfortably below today's detection rate and
        // above the count a collapsed walk could reach.
        let floor = SECURITY_RELEVANT_ROOTS.len() / 2;
        assert!(
            flagged.len() >= floor,
            "only {} of {} listed roots flagged (floor {floor}): detection \
             collapsed — flagged={flagged:?}",
            flagged.len(),
            SECURITY_RELEVANT_ROOTS.len()
        );
    }

    /// Every listed root must still exist in the schema, so a rename cannot
    /// leave a dead name protecting nothing (the `[workspace]` →
    /// `[project]` rename did exactly that).
    #[test]
    fn every_listed_security_root_exists_in_the_schema_or_is_a_known_alias() {
        let root = config_schema();
        let properties = &root
            .schema
            .object
            .as_ref()
            .expect("Config is an object schema")
            .properties;
        // `workspace` is the legacy spelling `config_compat` rewrites to
        // `project` before any walk; it is kept so a caller validating a
        // pre-normalization document is still protected.
        const ALIASES: &[&str] = &["workspace"];
        let dead: Vec<&&str> = SECURITY_RELEVANT_ROOTS
            .iter()
            .filter(|name| !properties.contains_key(**name) && !ALIASES.contains(name))
            .collect();
        assert!(dead.is_empty(), "dead entries protect nothing: {dead:?}");
    }
}
