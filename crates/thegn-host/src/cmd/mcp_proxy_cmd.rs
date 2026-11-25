//! `thegn mcp {status,reload,wire,secret,preset}` + the `emit --proxy` block.
//!
//! The stdio shim itself (`thegn mcp proxy`) is [`crate::mcp_proxy::run_shim`];
//! this module is the operator surface around it. The security-critical parts —
//! the secret-free wired entry, the default-deny exposure the status prints, the
//! keyring custody — are here; the pure merge/aggregate/filter they rely on is
//! in `thegn_core` (unit-tested).

use std::io::BufRead;
use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::{Map, Value, json};
use thegn_core::config::Config;
use thegn_core::mcp::wire::{self, WireOutcome};
use thegn_core::outln;

use super::mcp::{PresetAction, SecretAction};

/// The command an agent runs for the proxy — the current executable's path (so
/// a wired agent finds thegn even without it on PATH), falling back to `thegn`.
fn proxy_command() -> String {
    thegn_core::util::self_exe_str()
}

/// The `emit --proxy` block: the secret-free proxy entry, in the same
/// `{ name: { command, args } }` shape `settings_block` produces (no `env`).
pub fn proxy_emit_block() -> Value {
    let mut map = Map::new();
    map.insert(
        wire::ENTRY_KEY.to_string(),
        wire::proxy_entry(&proxy_command()),
    );
    Value::Object(map)
}

// ── status ──────────────────────────────────────────────────────────────────

/// Probe the configured upstreams live (spawn, handshake, filter) and report
/// per-upstream state. A live probe — like doctor — so counts are real.
pub fn status(cfg: &Config, as_json: bool) -> Result<()> {
    if !cfg.mcp_proxy.enabled {
        if as_json {
            outln!("{}", json!({ "enabled": false, "upstreams": [] }));
        } else {
            outln!("mcp proxy is disabled ([mcp_proxy] enabled = false)");
        }
        return Ok(());
    }
    let hub = crate::mcp_proxy::build_hub_for_cwd(cfg);
    let now = crate::mcp_proxy::now_ms();
    let reports = hub.reports(now);

    if as_json {
        let ups: Vec<Value> = reports
            .iter()
            .map(|r| {
                json!({
                    "name": r.name,
                    "scope": r.scope,
                    "partition_key": r.partition_key,
                    "running": r.running,
                    "breaker": r.breaker,
                    "health_age_ms": r.health_age_ms,
                    "exposed_tools": r.exposed.len(),
                    "hidden_tools": r.hidden.len(),
                    "exposed": r.exposed,
                    "withheld_reason": r.withheld_reason,
                    "error": r.error,
                })
            })
            .collect();
        outln!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "enabled": true,
                "advertised_tools": hub.tool_count(),
                "upstreams": ups,
            }))?
        );
        return Ok(());
    }

    if reports.is_empty() {
        outln!(
            "no exposed upstreams (default-deny — add [mcp_servers.<name>.proxy] tools = [...])"
        );
        return Ok(());
    }
    outln!("mcp proxy: {} tool(s) advertised", hub.tool_count());
    for r in &reports {
        let state = if let Some(reason) = &r.withheld_reason {
            format!("withheld — {reason}")
        } else if let Some(err) = &r.error {
            format!("error — {err}")
        } else if r.running {
            format!("running (breaker={})", r.breaker)
        } else {
            "not running".to_string()
        };
        outln!("{} [scope={}] {state}", r.name, r.scope);
        if let Some(k) = &r.partition_key {
            outln!("  partition: {k}");
        }
        if r.withheld_reason.is_none() && r.error.is_none() {
            outln!(
                "  exposed: {} ({}) | hidden: {}",
                r.exposed.len(),
                r.exposed.join(", "),
                r.hidden.len()
            );
        }
    }
    Ok(())
}

// ── reload ──────────────────────────────────────────────────────────────────

/// Ask the daemon to re-read config and reconcile its upstreams. Standalone
/// (no daemon) re-reads config on each `thegn mcp proxy` launch, so reload is a
/// daemon-only verb.
pub fn reload(cfg: &Config) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        match super::session::connect(cfg).await {
            Ok(client) => match client.mcp_proxy_reload().await {
                Ok(rep) => {
                    if rep.actions.is_empty() {
                        outln!("mcp proxy: already in sync (no changes)");
                    } else {
                        outln!("mcp proxy reconciled:");
                        for a in &rep.actions {
                            outln!("  {} {} [{}]", a.kind, a.upstream, a.partition_key);
                        }
                    }
                    if rep.tools_changed {
                        outln!("  → notified connected agents (tools/list_changed)");
                    }
                    Ok(())
                }
                Err(e) => bail!("reload failed: {e}"),
            },
            Err(_) => {
                outln!(
                    "no thegn daemon is running — the standalone proxy re-reads config on \
                     each `thegn mcp proxy` launch, so no reload is needed"
                );
                Ok(())
            }
        }
    })
}

// ── wire ──────────────────────────────────────────────────────────────────

use wire::WireFormat as Format;

/// A per-vendor MCP-settings adapter: where the file lives (under `$HOME`),
/// its format, the path to the object holding server entries, and the home
/// directory whose presence means the agent is installed. Vendor specifics are
/// confined here (the seam rule). Paths are best-effort per vendor; the merge
/// semantics (secret-free, marked, idempotent, non-clobbering) are the
/// unit-tested guarantee in `thegn_core::mcp::wire`.
#[derive(Debug)]
struct Adapter {
    kind: &'static str,
    rel_path: &'static str,
    format: Format,
    container: &'static [&'static str],
    /// Relative to `$HOME`: exists once the agent has been run or installed.
    detect: &'static str,
    /// Env var that relocates the agent's home (`CODEX_HOME`); `rel_path`'s
    /// first component is then replaced by its value.
    home_env: Option<&'static str>,
}

const ADAPTERS: &[Adapter] = &[
    Adapter {
        kind: "claude",
        rel_path: ".claude.json",
        format: Format::Json,
        container: &["mcpServers"],
        detect: ".claude",
        home_env: None,
    },
    Adapter {
        kind: "codex",
        rel_path: ".codex/config.toml",
        format: Format::Toml,
        container: &["mcp_servers"],
        detect: ".codex",
        home_env: Some("CODEX_HOME"),
    },
    Adapter {
        kind: "hermes",
        rel_path: ".hermes/config.yaml",
        format: Format::Yaml,
        container: &["mcp_servers"],
        detect: ".hermes",
        home_env: Some("HERMES_HOME"),
    },
    Adapter {
        kind: "cursor",
        rel_path: ".cursor/mcp.json",
        format: Format::Json,
        container: &["mcpServers"],
        detect: ".cursor",
        home_env: None,
    },
    Adapter {
        kind: "windsurf",
        rel_path: ".codeium/windsurf/mcp_config.json",
        format: Format::Json,
        container: &["mcpServers"],
        detect: ".codeium/windsurf",
        home_env: None,
    },
    Adapter {
        kind: "vscode",
        rel_path: ".config/Code/User/mcp.json",
        format: Format::Json,
        container: &["servers"],
        detect: ".config/Code",
        home_env: None,
    },
    Adapter {
        kind: "zed",
        rel_path: ".config/zed/settings.json",
        format: Format::Json,
        container: &["context_servers"],
        detect: ".config/zed",
        home_env: None,
    },
    Adapter {
        kind: "gemini",
        rel_path: ".gemini/settings.json",
        format: Format::Json,
        container: &["mcpServers"],
        detect: ".gemini",
        home_env: None,
    },
    Adapter {
        kind: "amp",
        rel_path: ".config/amp/settings.json",
        format: Format::Json,
        container: &["mcpServers"],
        detect: ".config/amp",
        home_env: None,
    },
];

fn adapter(kind: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.kind == kind)
}

fn known_kinds() -> String {
    ADAPTERS
        .iter()
        .map(|a| a.kind)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The settings file an adapter writes. `home_env` (when set to an absolute
/// path) relocates the agent home: `$CODEX_HOME/config.toml`.
fn settings_path(
    a: &Adapter,
    home: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> PathBuf {
    if let Some(var) = a.home_env
        && let Some(dir) = env(var).filter(|d| std::path::Path::new(d).is_absolute())
        && let Some((_, rest)) = a.rel_path.split_once('/')
    {
        return PathBuf::from(dir).join(rest);
    }
    home.join(a.rel_path)
}

/// Whether the agent looks installed for this user: its home directory (or
/// its relocated home) exists.
fn detected(a: &Adapter, home: &std::path::Path, env: &dyn Fn(&str) -> Option<String>) -> bool {
    if let Some(var) = a.home_env
        && let Some(dir) = env(var).filter(|d| std::path::Path::new(d).is_absolute())
    {
        return std::path::Path::new(&dir).is_dir();
    }
    home.join(a.detect).exists()
}

/// Which adapters a bare `thegn mcp wire` targets: every adapter whose kind
/// matches a configured `[[agents]]` name or provider, plus every agent that is
/// installed for this user — so wiring works on first run with no config.
fn default_targets(
    cfg: &Config,
    home: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Vec<&'static Adapter> {
    let configured: std::collections::HashSet<String> = cfg
        .agents
        .iter()
        .flat_map(|a| {
            let mut ids = vec![a.name.to_ascii_lowercase()];
            if let Some(p) = &a.provider {
                ids.push(p.to_ascii_lowercase());
            }
            ids
        })
        .collect();
    ADAPTERS
        .iter()
        .filter(|a| configured.contains(a.kind) || detected(a, home, env))
        .collect()
}

/// Wire (or `--remove`) thegn's MCP entries into agent CLI settings: the
/// aggregated proxy (`thegn`) and thegn's own docs/state server
/// (`thegn-serve`).
pub fn wire(cfg: &Config, agent: Option<&str>, all: bool, remove: bool) -> Result<()> {
    let home = thegn_core::util::home();
    let env = |k: &str| std::env::var(k).ok();
    let targets: Vec<&Adapter> = if let Some(kind) = agent {
        match adapter(kind) {
            Some(a) => vec![a],
            None => bail!("unknown agent `{kind}` — known: {}", known_kinds()),
        }
    } else if all {
        ADAPTERS.iter().collect()
    } else {
        let hits = default_targets(cfg, &home, &env);
        if hits.is_empty() {
            bail!(
                "no configured [[agents]] or installed agent matches a known adapter — \
                 pass --agent <kind> or --all (known: {})",
                known_kinds()
            );
        }
        hits
    };

    let command = proxy_command();
    for a in targets {
        for line in wire_target(a, &home, &env, &command, remove) {
            outln!("{line}");
        }
    }
    Ok(())
}

/// Wire (or remove) one agent end to end: its MCP entries, then its lifecycle
/// hooks. Returns the report lines; a refusal is a line, never an error, so
/// one agent's foreign entry never blocks the next.
fn wire_target(
    a: &Adapter,
    home: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
    command: &str,
    remove: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    let path = settings_path(a, home, env);
    match wire_file(a, &path, command, remove) {
        Ok(outcomes) => {
            for (key, outcome) in outcomes {
                lines.push(format!(
                    "{} {key}: {} ({})",
                    a.kind,
                    outcome.as_str(),
                    path.display()
                ));
            }
        }
        Err(e) => lines.push(format!("{}: skipped — {e}", a.kind)),
    }
    if let Some(hook_path) = hooks_path(a, home, env) {
        match wire_hooks_file(a, &hook_path, command, remove) {
            Ok(outcome) => lines.push(format!(
                "{} hooks: {} ({})",
                a.kind,
                outcome.as_str(),
                hook_path.display()
            )),
            Err(e) => lines.push(format!("{} hooks: skipped — {e}", a.kind)),
        }
    }
    lines
}

/// The agents that have lifecycle hooks thegn can write (`claude`, `codex`).
pub fn hook_capable(kind: &str) -> bool {
    matches!(kind, "claude" | "codex")
}

/// Agent kinds installed for this user that thegn can wire and that are not
/// fully wired yet (an MCP entry or a lifecycle hook is missing). The welcome
/// tour offers exactly these; an empty answer means there is nothing to offer.
pub fn unwired_detected_agents(cfg: &Config) -> Vec<String> {
    let home = thegn_core::util::home();
    let env = |k: &str| std::env::var(k).ok();
    let command = proxy_command();
    default_targets(cfg, &home, &env)
        .into_iter()
        .filter(|a| hook_capable(a.kind))
        .filter(|a| detected(a, &home, &env))
        .filter(|a| !is_wired(a, &home, &env, &command))
        .map(|a| a.kind.to_string())
        .collect()
}

/// Whether wiring the agent now would change nothing (dry run on the text).
fn is_wired(
    a: &Adapter,
    home: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
    command: &str,
) -> bool {
    let read = |p: &std::path::Path| std::fs::read_to_string(p).unwrap_or_default();
    // A file thegn cannot wire (unparseable, foreign entry) counts as done: an
    // offer to wire it could only fail.
    let mcp_ok = match wire::apply_all(
        a.format,
        &read(&settings_path(a, home, env)),
        a.container,
        command,
        &[],
        false,
    ) {
        Ok((_, outcomes)) => outcomes.iter().all(|(_, o)| *o == WireOutcome::Unchanged),
        Err(_) => true,
    };
    let hooks_ok = hooks_path(a, home, env).is_none_or(|p| {
        let body = read(&p);
        match a.kind {
            "claude" => {
                let mut v: Value =
                    serde_json::from_str(&body).unwrap_or_else(|_| Value::Object(Map::new()));
                wire::apply_claude_hooks(&mut v, command, false) == Ok(WireOutcome::Unchanged)
            }
            _ => {
                let mut doc: toml_edit::DocumentMut = body.parse().unwrap_or_default();
                wire::apply_codex_notify(&mut doc, command, false) == Ok(WireOutcome::Unchanged)
            }
        }
    });
    mcp_ok && hooks_ok
}

/// Wire the named agent kinds (MCP entries + lifecycle hooks), as the welcome
/// tour's one-key accept does. Returns one report line per write.
pub fn wire_kinds(kinds: &[String]) -> Result<Vec<String>> {
    let home = thegn_core::util::home();
    let env = |k: &str| std::env::var(k).ok();
    let command = proxy_command();
    let mut lines = Vec::new();
    for kind in kinds {
        let Some(a) = adapter(kind) else {
            bail!("unknown agent `{kind}` — known: {}", known_kinds());
        };
        lines.extend(wire_target(a, &home, &env, &command, false));
    }
    Ok(lines)
}

/// Where an agent's lifecycle-hook settings live: Claude's `settings.json`
/// (relocated by an absolute `CLAUDE_CONFIG_DIR`) or Codex's `config.toml`.
fn hooks_path(
    a: &Adapter,
    home: &std::path::Path,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<PathBuf> {
    match a.kind {
        "claude" => Some(
            env("CLAUDE_CONFIG_DIR")
                .filter(|d| std::path::Path::new(d).is_absolute())
                .map(|d| PathBuf::from(d).join("settings.json"))
                .unwrap_or_else(|| home.join(".claude/settings.json")),
        ),
        "codex" => Some(settings_path(a, home, env)),
        _ => None,
    }
}

/// Apply thegn's lifecycle hooks to one agent's settings file. Same rules as
/// [`wire_file`]: nothing is written unless something changed, an unparseable
/// file is refused, and `--remove` never creates a file.
fn wire_hooks_file(
    a: &Adapter,
    path: &std::path::Path,
    command: &str,
    remove: bool,
) -> Result<WireOutcome> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("cannot read {}: {e}", path.display()),
    };
    if remove && body.is_none() {
        return Ok(WireOutcome::NothingToRemove);
    }
    let body = body.unwrap_or_default();
    let (outcome, rendered) = match a.kind {
        "claude" => {
            let mut settings: Value = if body.trim().is_empty() {
                Value::Object(Map::new())
            } else {
                serde_json::from_str(&body)
                    .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?
            };
            let outcome = wire::apply_claude_hooks(&mut settings, command, remove)
                .map_err(|e| anyhow::anyhow!(e))?;
            (outcome, serde_json::to_string_pretty(&settings)?)
        }
        _ => {
            let mut doc: toml_edit::DocumentMut = body
                .parse()
                .map_err(|e| anyhow::anyhow!("{} is not valid TOML: {e}", path.display()))?;
            let outcome = wire::apply_codex_notify(&mut doc, command, remove)
                .map_err(|e| anyhow::anyhow!(e))?;
            (outcome, doc.to_string())
        }
    };
    if !matches!(
        outcome,
        WireOutcome::Unchanged | WireOutcome::NothingToRemove
    ) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, rendered)?;
    }
    Ok(outcome)
}

/// Apply every [`wire::WIRED_ENTRIES`] row to one settings file, writing it
/// only when something changed. A file that does not parse is refused — never
/// clobbered — and a foreign entry of the same name fails the whole file
/// before anything is written.
fn wire_file(
    a: &Adapter,
    path: &std::path::Path,
    command: &str,
    remove: bool,
) -> Result<Vec<(&'static str, WireOutcome)>> {
    let body = match std::fs::read_to_string(path) {
        Ok(body) => Some(body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => bail!("cannot read {}: {e}", path.display()),
    };
    if remove && body.is_none() {
        return Ok(wire::WIRED_ENTRIES
            .iter()
            .map(|(key, _)| (*key, WireOutcome::NothingToRemove))
            .collect());
    }
    let body = body.unwrap_or_default();

    let (rendered, outcomes) = wire::apply_all(a.format, &body, a.container, command, &[], remove)
        .map_err(|e| anyhow::anyhow!("{}: {e}", path.display()))?;

    let changed = outcomes
        .iter()
        .any(|(_, o)| !matches!(o, WireOutcome::Unchanged | WireOutcome::NothingToRemove));
    if changed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, rendered)?;
    }
    Ok(outcomes)
}

// ── secret ──────────────────────────────────────────────────────────────────

/// Manage keyring entries the proxy resolves at spawn. Backed by the canonical
/// `SecretStore` seam (`crate::secret`) plus the MCP names-only index.
pub fn secret(action: SecretAction) -> Result<()> {
    match action {
        SecretAction::Set { name, value } => {
            let value = match value {
                Some(v) => v,
                None => {
                    // Read one line from stdin — keeps the value out of argv /
                    // shell history.
                    let mut line = String::new();
                    std::io::stdin().lock().read_line(&mut line)?;
                    let v = line.trim_end_matches(['\n', '\r']).to_string();
                    if v.is_empty() {
                        bail!("no value provided on stdin");
                    }
                    v
                }
            };
            crate::secret::mcp_secret_set(&name, &value).map_err(|e| anyhow::anyhow!("{e}"))?;
            outln!("stored keyring:{name} (agents resolve it only at spawn)");
            Ok(())
        }
        SecretAction::Rm { name } => {
            crate::secret::mcp_secret_rm(&name).map_err(|e| anyhow::anyhow!("{e}"))?;
            outln!("removed keyring:{name}");
            Ok(())
        }
        SecretAction::List => {
            let names = crate::secret::mcp_secret_list().map_err(|e| anyhow::anyhow!("{e}"))?;
            if names.is_empty() {
                outln!("no thegn-managed keyring entries");
            } else {
                for n in names {
                    outln!("keyring:{n}");
                }
            }
            Ok(())
        }
    }
}

// ── preset ──────────────────────────────────────────────────────────────────

/// List / show curated presets; `--write` appends after printing.
pub fn preset(_cfg: &Config, config_path: PathBuf, action: PresetAction) -> Result<()> {
    use thegn_core::mcp::presets;
    match action {
        PresetAction::List => {
            for p in presets::PRESETS {
                let req = if p.requires.is_empty() {
                    "local (no API key)".to_string()
                } else {
                    format!("requires: {}", p.requires.join("; "))
                };
                outln!("{} [{}] — {}", p.name, p.category, req);
                outln!("  {}", p.description);
            }
            Ok(())
        }
        PresetAction::Show { name, write } => {
            let Some(p) = presets::find(&name) else {
                bail!("no such preset `{name}` (see `thegn mcp preset list`)");
            };
            // Always print first — presets never silently edit config.
            outln!("{}", p.toml);
            if write {
                append_preset_to_config(&config_path, p)?;
                outln!(
                    "# appended [mcp_servers.{}] to {}",
                    p.name,
                    config_path.display()
                );
                if !p.requires.is_empty() {
                    outln!("# NOTE: this preset needs — {}", p.requires.join("; "));
                }
            }
            Ok(())
        }
    }
}

fn append_preset_to_config(
    config_path: &std::path::Path,
    p: &thegn_core::mcp::presets::Preset,
) -> Result<()> {
    let existing = std::fs::read_to_string(config_path).unwrap_or_default();
    let marker = format!("[mcp_servers.{}]", p.name);
    if existing.contains(&marker) {
        bail!("config already has {marker} — edit it directly rather than appending a duplicate");
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push('\n');
    body.push_str(p.toml);
    std::fs::write(config_path, body)?;
    Ok(())
}

#[cfg(test)]
mod wire_tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn every_adapter_kind_is_unique_and_codex_is_toml_hermes_yaml() {
        let kinds: std::collections::HashSet<&str> = ADAPTERS.iter().map(|a| a.kind).collect();
        assert_eq!(kinds.len(), ADAPTERS.len());
        let codex = adapter("codex").unwrap();
        assert_eq!(codex.format, Format::Toml);
        assert_eq!(codex.container, &["mcp_servers"]);
        let hermes = adapter("hermes").unwrap();
        assert_eq!(hermes.format, Format::Yaml);
        assert_eq!(hermes.container, &["mcp_servers"]);
        for a in ADAPTERS
            .iter()
            .filter(|a| a.kind != "codex" && a.kind != "hermes")
        {
            assert_eq!(a.format, Format::Json, "{}", a.kind);
        }
    }

    #[test]
    fn codex_home_relocates_the_settings_file() {
        let home = std::path::Path::new("/home/u");
        let codex = adapter("codex").unwrap();
        assert_eq!(
            settings_path(codex, home, &no_env),
            PathBuf::from("/home/u/.codex/config.toml")
        );
        let relocated = |k: &str| (k == "CODEX_HOME").then(|| "/srv/codex".to_string());
        assert_eq!(
            settings_path(codex, home, &relocated),
            PathBuf::from("/srv/codex/config.toml")
        );
        // A relative CODEX_HOME is ignored rather than resolved against cwd.
        let relative = |k: &str| (k == "CODEX_HOME").then(|| "codex".to_string());
        assert_eq!(
            settings_path(codex, home, &relative),
            PathBuf::from("/home/u/.codex/config.toml")
        );
        let claude = adapter("claude").unwrap();
        assert_eq!(
            settings_path(claude, home, &relocated),
            PathBuf::from("/home/u/.claude.json")
        );
    }

    #[test]
    fn default_targets_include_installed_agents_without_config() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".codex")).unwrap();
        let kinds: Vec<&str> = default_targets(&Config::default(), tmp.path(), &no_env)
            .iter()
            .map(|a| a.kind)
            .collect();
        assert!(kinds.contains(&"claude"), "{kinds:?}");
        assert!(kinds.contains(&"codex"), "{kinds:?}");
        assert!(!kinds.contains(&"cursor"), "{kinds:?}");
    }

    #[test]
    fn claude_json_wire_is_idempotent_reversible_and_keeps_user_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude.json");
        let user = r#"{"theme":"dark","mcpServers":{"git":{"command":"git-mcp"}}}"#;
        std::fs::write(&path, user).unwrap();
        let claude = adapter("claude").unwrap();

        let first = wire_file(claude, &path, "/bin/thegn", false).unwrap();
        assert_eq!(
            first,
            vec![
                ("thegn", WireOutcome::Added),
                ("thegn-serve", WireOutcome::Added)
            ]
        );
        let wired: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(wired["theme"], "dark");
        assert_eq!(wired["mcpServers"]["git"]["command"], "git-mcp");
        assert_eq!(
            wired["mcpServers"]["thegn"]["args"],
            json!(["mcp", "proxy"])
        );
        assert_eq!(
            wired["mcpServers"]["thegn-serve"]["args"],
            json!(["mcp", "serve"])
        );
        assert!(wired["mcpServers"]["thegn-serve"].get("env").is_none());

        let before = std::fs::read_to_string(&path).unwrap();
        let again = wire_file(claude, &path, "/bin/thegn", false).unwrap();
        assert!(again.iter().all(|(_, o)| *o == WireOutcome::Unchanged));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);

        let removed = wire_file(claude, &path, "/bin/thegn", true).unwrap();
        assert!(removed.iter().all(|(_, o)| *o == WireOutcome::Removed));
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let original: Value = serde_json::from_str(user).unwrap();
        assert_eq!(back, original);
    }

    #[test]
    fn foreign_entry_refuses_the_whole_file_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude.json");
        let user = r#"{"mcpServers":{"thegn-serve":{"command":"mine"}}}"#;
        std::fs::write(&path, user).unwrap();
        let err = wire_file(adapter("claude").unwrap(), &path, "/bin/thegn", false).unwrap_err();
        assert!(err.to_string().contains("refusing to overwrite"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user);
    }

    #[test]
    fn unparseable_settings_are_refused_not_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join(".claude.json");
        std::fs::write(&json_path, "{ not json").unwrap();
        assert!(wire_file(adapter("claude").unwrap(), &json_path, "t", false).is_err());
        assert_eq!(std::fs::read_to_string(&json_path).unwrap(), "{ not json");

        let toml_path = tmp.path().join("config.toml");
        std::fs::write(&toml_path, "model = [").unwrap();
        assert!(wire_file(adapter("codex").unwrap(), &toml_path, "t", false).is_err());
        assert_eq!(std::fs::read_to_string(&toml_path).unwrap(), "model = [");
    }

    #[test]
    fn codex_toml_wire_round_trips_and_remove_on_absent_file_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".codex/config.toml");
        let codex = adapter("codex").unwrap();

        let none = wire_file(codex, &path, "/bin/thegn", true).unwrap();
        assert!(none.iter().all(|(_, o)| *o == WireOutcome::NothingToRemove));
        assert!(!path.exists(), "remove must not create the file");

        let added = wire_file(codex, &path, "/bin/thegn", false).unwrap();
        assert!(added.iter().all(|(_, o)| *o == WireOutcome::Added));
        let text = std::fs::read_to_string(&path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["mcp_servers"]["thegn"]["command"].as_str(),
            Some("/bin/thegn")
        );
        assert_eq!(
            parsed["mcp_servers"]["thegn-serve"]["args"][1].as_str(),
            Some("serve")
        );

        let again = wire_file(codex, &path, "/bin/thegn", false).unwrap();
        assert!(again.iter().all(|(_, o)| *o == WireOutcome::Unchanged));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn claude_hooks_round_trip_on_disk_and_remove_on_absent_file_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".claude/settings.json");
        let claude = adapter("claude").unwrap();
        assert_eq!(
            wire_hooks_file(claude, &path, "/bin/thegn", true).unwrap(),
            WireOutcome::NothingToRemove
        );
        assert!(!path.exists());
        assert_eq!(
            wire_hooks_file(claude, &path, "/bin/thegn", false).unwrap(),
            WireOutcome::Added
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"]
                .as_str()
                .unwrap()
                .contains("agent hook claude stop")
        );
        let before = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            wire_hooks_file(claude, &path, "/bin/thegn", false).unwrap(),
            WireOutcome::Unchanged
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        assert_eq!(
            wire_hooks_file(claude, &path, "/bin/thegn", true).unwrap(),
            WireOutcome::Removed
        );
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back, json!({}));
    }

    #[test]
    fn codex_notify_shares_config_toml_with_the_mcp_tables_and_refuses_foreign() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.toml");
        let codex = adapter("codex").unwrap();
        wire_file(codex, &path, "/bin/thegn", false).unwrap();
        assert_eq!(
            wire_hooks_file(codex, &path, "/bin/thegn", false).unwrap(),
            WireOutcome::Added
        );
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed["notify"][2].as_str(), Some("hook"));
        assert_eq!(
            parsed["mcp_servers"]["thegn"]["command"].as_str(),
            Some("/bin/thegn")
        );
        wire_hooks_file(codex, &path, "/bin/thegn", true).unwrap();
        wire_file(codex, &path, "/bin/thegn", true).unwrap();

        let theirs = tmp.path().join("theirs.toml");
        std::fs::write(&theirs, "notify = [\"mine\"]\n").unwrap();
        assert!(wire_hooks_file(codex, &theirs, "/bin/thegn", false).is_err());
        assert_eq!(
            std::fs::read_to_string(&theirs).unwrap(),
            "notify = [\"mine\"]\n"
        );
    }

    #[test]
    fn unwired_detection_flips_once_wired() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        let claude = adapter("claude").unwrap();
        assert!(!is_wired(claude, tmp.path(), &no_env, "/bin/thegn"));
        let lines = wire_target(claude, tmp.path(), &no_env, "/bin/thegn", false);
        assert!(
            lines.iter().any(|l| l.starts_with("claude hooks: added")),
            "{lines:?}"
        );
        assert!(is_wired(claude, tmp.path(), &no_env, "/bin/thegn"));
        wire_target(claude, tmp.path(), &no_env, "/bin/thegn", true);
        assert!(!is_wired(claude, tmp.path(), &no_env, "/bin/thegn"));
    }

    #[test]
    fn hermes_home_relocates_and_is_detected() {
        let home = std::path::Path::new("/home/u");
        let hermes = adapter("hermes").unwrap();
        assert_eq!(
            settings_path(hermes, home, &no_env),
            PathBuf::from("/home/u/.hermes/config.yaml")
        );
        let relocated = |k: &str| (k == "HERMES_HOME").then(|| "/srv/hermes".to_string());
        assert_eq!(
            settings_path(hermes, home, &relocated),
            PathBuf::from("/srv/hermes/config.yaml")
        );
        let tmp = tempfile::tempdir().unwrap();
        assert!(!detected(hermes, tmp.path(), &no_env));
        std::fs::create_dir_all(tmp.path().join(".hermes")).unwrap();
        assert!(detected(hermes, tmp.path(), &no_env));
        let kinds: Vec<&str> = default_targets(&Config::default(), tmp.path(), &no_env)
            .iter()
            .map(|a| a.kind)
            .collect();
        assert_eq!(kinds, vec!["hermes"]);
    }

    #[test]
    fn hermes_yaml_wire_is_idempotent_reversible_and_keeps_user_text() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".hermes/config.yaml");
        let hermes = adapter("hermes").unwrap();

        let none = wire_file(hermes, &path, "/bin/thegn", true).unwrap();
        assert!(none.iter().all(|(_, o)| *o == WireOutcome::NothingToRemove));
        assert!(!path.exists(), "remove must not create the file");

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let user = "# my hermes\nmodel: nous-hermes\nmcp_servers:\n  git:\n    command: git-mcp\n";
        std::fs::write(&path, user).unwrap();
        let added = wire_file(hermes, &path, "/bin/thegn", false).unwrap();
        assert_eq!(
            added,
            vec![
                ("thegn", WireOutcome::Added),
                ("thegn-serve", WireOutcome::Added)
            ]
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.starts_with("# my hermes\nmodel: nous-hermes\n"),
            "{text}"
        );
        assert!(text.contains("  git:\n    command: git-mcp\n"), "{text}");
        assert!(text.contains("command: \"/bin/thegn\""), "{text}");

        let again = wire_file(hermes, &path, "/bin/thegn", false).unwrap();
        assert!(again.iter().all(|(_, o)| *o == WireOutcome::Unchanged));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);

        let removed = wire_file(hermes, &path, "/bin/thegn", true).unwrap();
        assert!(removed.iter().all(|(_, o)| *o == WireOutcome::Removed));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user);
    }

    #[test]
    fn hermes_foreign_entry_is_refused_without_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("config.yaml");
        let user = "mcp_servers:\n  thegn-serve:\n    command: mine\n";
        std::fs::write(&path, user).unwrap();
        let hermes = adapter("hermes").unwrap();
        assert!(wire_file(hermes, &path, "/bin/thegn", false).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), user);
    }
}
