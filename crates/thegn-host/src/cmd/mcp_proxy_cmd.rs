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

/// The `emit --proxy` block: the single secret-free proxy entry, in the same
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

// ── wire ────────────────────────────────────────────────────────────────────

/// A per-vendor MCP-settings adapter: where the file lives (under `$HOME`) and
/// the JSON path to the object holding server entries. Vendor specifics are
/// confined here (the seam rule). Paths are best-effort per vendor; the merge
/// semantics (secret-free, marked, idempotent, non-clobbering) are the
/// unit-tested guarantee.
struct Adapter {
    kind: &'static str,
    rel_path: &'static str,
    container: &'static [&'static str],
}

const ADAPTERS: &[Adapter] = &[
    Adapter {
        kind: "claude",
        rel_path: ".claude.json",
        container: &["mcpServers"],
    },
    Adapter {
        kind: "cursor",
        rel_path: ".cursor/mcp.json",
        container: &["mcpServers"],
    },
    Adapter {
        kind: "windsurf",
        rel_path: ".codeium/windsurf/mcp_config.json",
        container: &["mcpServers"],
    },
    Adapter {
        kind: "vscode",
        rel_path: ".config/Code/User/mcp.json",
        container: &["servers"],
    },
    Adapter {
        kind: "zed",
        rel_path: ".config/zed/settings.json",
        container: &["context_servers"],
    },
    Adapter {
        kind: "gemini",
        rel_path: ".gemini/settings.json",
        container: &["mcpServers"],
    },
    Adapter {
        kind: "amp",
        rel_path: ".config/amp/settings.json",
        container: &["mcpServers"],
    },
];

fn adapter(kind: &str) -> Option<&'static Adapter> {
    ADAPTERS.iter().find(|a| a.kind == kind)
}

/// Wire (or `--remove`) the proxy entry into agent CLI settings.
pub fn wire(cfg: &Config, agent: Option<&str>, all: bool, remove: bool) -> Result<()> {
    // codex uses a TOML config, not the JSON `mcpServers` shape — call it out
    // rather than silently skipping, so the user isn't left wondering.
    if agent == Some("codex") {
        outln!(
            "codex uses a TOML config (~/.codex/config.toml) — add this block by hand:\n\n\
             [mcp_servers.thegn]\ncommand = \"{}\"\nargs = [\"mcp\", \"proxy\"]",
            proxy_command()
        );
        return Ok(());
    }

    let targets: Vec<&Adapter> = if let Some(kind) = agent {
        match adapter(kind) {
            Some(a) => vec![a],
            None => bail!(
                "unknown agent `{kind}` — known: {} (codex uses TOML; wire it by hand)",
                ADAPTERS
                    .iter()
                    .map(|a| a.kind)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    } else if all {
        ADAPTERS.iter().collect()
    } else {
        // Default: adapters whose kind matches a configured `[[agents]]`
        // (source of truth). Match by the agent's name or provider.
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
        let hits: Vec<&Adapter> = ADAPTERS
            .iter()
            .filter(|a| configured.contains(a.kind))
            .collect();
        if hits.is_empty() {
            bail!(
                "no configured [[agents]] match a known adapter — pass --agent <kind> \
                 or --all (known: {})",
                ADAPTERS
                    .iter()
                    .map(|a| a.kind)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        hits
    };

    let command = proxy_command();
    for a in targets {
        match wire_one(a, &command, remove) {
            Ok((outcome, path)) => {
                outln!("{}: {} ({})", a.kind, outcome.as_str(), path.display());
            }
            Err(e) => outln!("{}: skipped — {e}", a.kind),
        }
    }
    Ok(())
}

fn wire_one(a: &Adapter, command: &str, remove: bool) -> Result<(WireOutcome, PathBuf)> {
    let home = thegn_core::util::home();
    let path = home.join(a.rel_path);

    // Read existing settings (or an empty object if absent). A parse error is a
    // refusal — never clobber a file we cannot understand.
    let mut settings: Value = match std::fs::read_to_string(&path) {
        Ok(body) if body.trim().is_empty() => Value::Object(Map::new()),
        Ok(body) => serde_json::from_str(&body)
            .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?,
        Err(_) => Value::Object(Map::new()),
    };

    let entry = wire::proxy_entry(command);
    let outcome =
        wire::apply(&mut settings, a.container, entry, remove).map_err(|e| anyhow::anyhow!(e))?;

    // Only touch the file if something changed.
    if !matches!(
        outcome,
        WireOutcome::Unchanged | WireOutcome::NothingToRemove
    ) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_string_pretty(&settings)?;
        std::fs::write(&path, body)?;
    }
    Ok((outcome, path))
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
            let names = crate::secret::mcp_secret_list();
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
