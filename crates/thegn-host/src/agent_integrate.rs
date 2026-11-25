//! Out-of-the-box agent integration inside a remote environment (THE-2714).
//!
//! After the provisioning steps that install the coding agents and carry their
//! logins, this wires each agent in the sandbox the same way `thegn mcp wire`
//! does on the host, and seeds the everyday `thegn` skill into the agent's
//! user-level skill folder:
//!
//! * **MCP.** Each agent's settings file in the sandbox `$HOME` gets thegn's two
//!   marked, argv-only entries (`thegn`, `thegn-serve`). The command is the
//!   in-sandbox resolver (`wire::SANDBOX_COMMAND`), never the host's path: the
//!   settings carried up from the host name a path that does not exist there,
//!   and the pass rewrites exactly the entries thegn marked. A foreign entry of
//!   the same name, or a file that does not parse, is left untouched.
//! * **Skill.** `<agent home>/skills/thegn/SKILL.md`, written only when absent
//!   or an intact managed copy (the same proof `plan_seed` uses); a file the
//!   user wrote or edited is never replaced.
//!
//! Idempotent: every write compares first, so a re-provision, a snapshot
//! restore or a second open changes nothing. Best-effort: a failure is a
//! warning, never a failed provisioning step.
//!
//! * **Hooks.** Claude's `~/.claude/settings.json` `Stop`/`Notification` groups
//!   and Codex's `notify` in `config.toml`, with the in-sandbox resolver as the
//!   command (a hook from the carried host settings is rewritten; a hook the
//!   user wrote is never touched).

use thegn_core::mcp::wire::{self, WireFormat};
use thegn_core::skills::{self, SkillDocument};
use thegn_svc::provider::Provider;

use crate::agent::block_on_provider;

/// Where one agent keeps its MCP settings and user-level skills, relative to
/// the sandbox `$HOME`.
#[derive(Debug, PartialEq, Eq)]
struct RemoteLayout {
    /// `(settings file, format, container path)`; `None` for an agent with no
    /// MCP client (pi).
    mcp: Option<(&'static str, WireFormat, &'static [&'static str])>,
    /// The user-level skills directory.
    skills: &'static str,
    /// Lifecycle hooks: `(file, kind)`; `None` where the agent has none.
    hooks: Option<(&'static str, HookKind)>,
}

/// How an agent's lifecycle hooks are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HookKind {
    /// Claude `settings.json` hook groups.
    ClaudeJson,
    /// Codex top-level `notify` in `config.toml`.
    CodexNotify,
}

fn layout(agent: &str) -> Option<RemoteLayout> {
    Some(match agent.trim() {
        "claude" | "claude-code" => RemoteLayout {
            mcp: Some((".claude.json", WireFormat::Json, &["mcpServers"])),
            skills: ".claude/skills",
            hooks: Some((".claude/settings.json", HookKind::ClaudeJson)),
        },
        "codex" => RemoteLayout {
            mcp: Some((".codex/config.toml", WireFormat::Toml, &["mcp_servers"])),
            skills: ".codex/skills",
            hooks: Some((".codex/config.toml", HookKind::CodexNotify)),
        },
        "pi" => RemoteLayout {
            mcp: None,
            skills: ".pi/agent/skills",
            hooks: None,
        },
        "hermes" => RemoteLayout {
            mcp: Some((".hermes/config.yaml", WireFormat::Yaml, &["mcp_servers"])),
            skills: ".hermes/skills",
            hooks: None,
        },
        _ => return None,
    })
}

/// The new text of a sandbox settings file after wiring, or `None` when it is
/// already wired exactly so (nothing to write). `Err` for a foreign entry or
/// unparseable text.
fn merged_settings(
    format: WireFormat,
    container: &[&str],
    body: &str,
) -> Result<Option<String>, String> {
    let (text, outcomes) = wire::apply_all(
        format,
        body,
        container,
        wire::SANDBOX_COMMAND,
        wire::SANDBOX_PREFIX,
        false,
    )?;
    let changed = outcomes
        .iter()
        .any(|(_, o)| !matches!(o, wire::WireOutcome::Unchanged));
    Ok(changed.then_some(text))
}

/// The new text of a sandbox hooks file, or `None` when already installed
/// exactly so. `Err` for a user-owned `notify`/hook container or bad syntax.
fn merged_hooks(kind: HookKind, body: &str) -> Result<Option<String>, String> {
    let (text, outcome) = match kind {
        HookKind::ClaudeJson => {
            let mut settings: serde_json::Value = if body.trim().is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(body).map_err(|e| format!("not valid JSON: {e}"))?
            };
            let o = wire::apply_sandbox_claude_hooks(&mut settings, false)?;
            (
                serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?,
                o,
            )
        }
        HookKind::CodexNotify => {
            let mut doc: toml_edit::DocumentMut =
                body.parse().map_err(|e| format!("not valid TOML: {e}"))?;
            let o = wire::apply_sandbox_codex_notify(&mut doc, false)?;
            (doc.to_string(), o)
        }
    };
    Ok((outcome != wire::WireOutcome::Unchanged).then_some(text))
}

/// What to write for the skill given the sandbox file's current bytes: `None`
/// when a user-owned file or an intact current copy is there.
fn skill_to_write(existing: Option<&[u8]>, skill: &SkillDocument) -> Option<String> {
    let Some(bytes) = existing else {
        return Some(skills::render_managed(skill));
    };
    let managed = skills::inspect_managed(bytes)?;
    if managed.is_user_modified() || managed.marker.recorded_hash == skills::document_hash(skill) {
        return None;
    }
    Some(skills::render_managed(skill))
}

/// Read a sandbox file: `Ok(None)` only when it is verifiably absent, so a
/// transient failure is an error and never mistaken for an empty file.
fn read_if_exists(provider: &Provider, id: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let argv = ["/bin/sh", "-c", "test -e \"$1\"", "sh", path].map(String::from);
    let (code, _) = block_on_provider(|| async { provider.run_exec(id, &argv, None, &[]).await })?;
    match code {
        0 => Ok(Some(block_on_provider(|| async {
            provider.read(id, path).await
        })?)),
        1 => Ok(None),
        other => anyhow::bail!("cannot probe {path} (exit {other})"),
    }
}

fn mkdir_parent(provider: &Provider, id: &str, path: &str) -> anyhow::Result<()> {
    let Some((dir, _)) = path.rsplit_once('/') else {
        return Ok(());
    };
    let argv = ["/bin/sh", "-c", "mkdir -p \"$1\"", "sh", dir].map(String::from);
    let (code, out) =
        block_on_provider(|| async { provider.run_exec(id, &argv, None, &[]).await })?;
    anyhow::ensure!(code == 0, "mkdir {dir} (exit {code}): {}", out.trim());
    Ok(())
}

fn wire_settings(
    provider: &Provider,
    id: &str,
    path: &str,
    format: WireFormat,
    container: &[&str],
) -> anyhow::Result<()> {
    let existing = read_if_exists(provider, id, path)?;
    let body = existing
        .map(|b| String::from_utf8(b).map_err(|_| anyhow::anyhow!("{path} is not UTF-8")))
        .transpose()?
        .unwrap_or_default();
    let merged = merged_settings(format, container, &body).map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(text) = merged {
        mkdir_parent(provider, id, path)?;
        // Private like the login files beside it: a settings file may hold
        // account state, and an unchanged file never reaches here.
        block_on_provider(|| async { provider.write_private(id, path, text.as_bytes()).await })?;
    }
    Ok(())
}

fn wire_hooks(provider: &Provider, id: &str, path: &str, kind: HookKind) -> anyhow::Result<()> {
    let existing = read_if_exists(provider, id, path)?;
    let body = existing
        .map(|b| String::from_utf8(b).map_err(|_| anyhow::anyhow!("{path} is not UTF-8")))
        .transpose()?
        .unwrap_or_default();
    if let Some(text) = merged_hooks(kind, &body).map_err(|e| anyhow::anyhow!("{e}"))? {
        mkdir_parent(provider, id, path)?;
        block_on_provider(|| async { provider.write_private(id, path, text.as_bytes()).await })?;
    }
    Ok(())
}

fn seed_skill(
    provider: &Provider,
    id: &str,
    path: &str,
    skill: &SkillDocument,
) -> anyhow::Result<()> {
    let existing = read_if_exists(provider, id, path)?;
    if let Some(text) = skill_to_write(existing.as_deref(), skill) {
        mkdir_parent(provider, id, path)?;
        block_on_provider(|| async { provider.write(id, path, text.as_bytes()).await })?;
    }
    Ok(())
}

/// Wire MCP and seed the `thegn` skill for every known agent in `agents`
/// inside the sandbox whose `$HOME` is `home`. `seed_skills` mirrors
/// `[skills] enabled`. Best-effort: failures are warned, never returned.
pub(crate) fn integrate_sandbox_agents(
    provider: &Provider,
    id: &str,
    home: &str,
    agents: &[String],
    seed_skills: bool,
) {
    let home = home.trim_end_matches('/');
    let skill = if seed_skills {
        skills::SkillRegistry::embedded()
            .ok()
            .and_then(|r| r.get("thegn").cloned())
    } else {
        None
    };
    let mut done = std::collections::BTreeSet::new();
    for agent in agents {
        let Some(lay) = layout(agent) else { continue };
        if !done.insert(lay.skills) {
            continue;
        }
        if let Some((rel, format, container)) = lay.mcp
            && let Err(e) = wire_settings(provider, id, &format!("{home}/{rel}"), format, container)
        {
            thegn_core::msg::warn(&format!(
                "agent integration ({agent}): mcp wire skipped: {e}"
            ));
        }
        if let Some((rel, kind)) = lay.hooks
            && let Err(e) = wire_hooks(provider, id, &format!("{home}/{rel}"), kind)
        {
            thegn_core::msg::warn(&format!("agent integration ({agent}): hooks skipped: {e}"));
        }
        if let Some(skill) = &skill {
            let path = format!("{home}/{}/{}/SKILL.md", lay.skills, skill.name);
            if let Err(e) = seed_skill(provider, id, &path, skill) {
                thegn_core::msg::warn(&format!("agent integration ({agent}): skill skipped: {e}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thegn_skill() -> SkillDocument {
        skills::SkillRegistry::embedded()
            .unwrap()
            .get("thegn")
            .unwrap()
            .clone()
    }

    #[test]
    fn every_installable_agent_has_a_layout_and_pi_has_no_mcp() {
        for kind in ["claude", "claude-code", "codex", "pi", "hermes"] {
            assert!(layout(kind).is_some(), "{kind}");
        }
        assert!(layout("gemini").is_none());
        assert_eq!(layout("pi").unwrap().mcp, None);
        let (rel, format, container) = layout("hermes").unwrap().mcp.unwrap();
        assert_eq!(
            (rel, format, container),
            (
                ".hermes/config.yaml",
                WireFormat::Yaml,
                &["mcp_servers"][..]
            )
        );
    }

    #[test]
    fn settings_merge_is_idempotent_and_never_names_a_host_path() {
        for (format, container, host) in [
            (
                WireFormat::Json,
                &["mcpServers"][..],
                "{\"theme\":\"dark\"}",
            ),
            (WireFormat::Toml, &["mcp_servers"][..], "model = \"x\"\n"),
            (WireFormat::Yaml, &["mcp_servers"][..], "model: x\n"),
        ] {
            let first = merged_settings(format, container, host).unwrap().unwrap();
            assert!(first.contains("/bin/sh"), "{first}");
            assert!(!first.contains("/home/"), "{first}");
            assert_eq!(
                merged_settings(format, container, &first).unwrap(),
                None,
                "second pass is a no-op ({format:?})"
            );
        }
    }

    #[test]
    fn host_wired_entries_are_rewritten_and_foreign_ones_refused() {
        let (host, _) = wire::apply_all(
            WireFormat::Json,
            "{}",
            &["mcpServers"],
            "/home/u/.nix-profile/bin/thegn",
            &[],
            false,
        )
        .unwrap();
        let fixed = merged_settings(WireFormat::Json, &["mcpServers"], &host)
            .unwrap()
            .unwrap();
        assert!(!fixed.contains("/home/u"), "{fixed}");
        let foreign = "{\"mcpServers\":{\"thegn\":{\"command\":\"mine\"}}}";
        assert!(merged_settings(WireFormat::Json, &["mcpServers"], foreign).is_err());
        assert!(merged_settings(WireFormat::Json, &["mcpServers"], "{ nope").is_err());
    }

    #[test]
    fn sandbox_hooks_are_idempotent_rewrite_host_paths_and_spare_user_notify() {
        let first = merged_hooks(HookKind::ClaudeJson, "").unwrap().unwrap();
        assert!(first.contains("x-thegn-managed") && !first.contains("/home/"));
        assert_eq!(merged_hooks(HookKind::ClaudeJson, &first), Ok(None));
        let mut host: serde_json::Value = serde_json::json!({});
        wire::apply_claude_hooks(&mut host, "/home/u/thegn", false).unwrap();
        let fixed = merged_hooks(HookKind::ClaudeJson, &host.to_string())
            .unwrap()
            .unwrap();
        assert!(!fixed.contains("/home/u"), "{fixed}");
        let toml = merged_hooks(HookKind::CodexNotify, "model = \"x\"\n")
            .unwrap()
            .unwrap();
        assert!(toml.contains("notify") && toml.contains("model"));
        assert_eq!(merged_hooks(HookKind::CodexNotify, &toml), Ok(None));
        assert!(merged_hooks(HookKind::CodexNotify, "notify = [\"mine\"]\n").is_err());
        assert!(layout("pi").unwrap().hooks.is_none());
    }

    #[test]
    fn skill_is_written_once_and_never_over_a_user_file() {
        let skill = thegn_skill();
        let first = skill_to_write(None, &skill).expect("absent -> write");
        assert!(first.contains("thegn_managed: true"));
        assert_eq!(skill_to_write(Some(first.as_bytes()), &skill), None);
        assert_eq!(skill_to_write(Some(b"my own skill"), &skill), None);
        let edited = first.replace("everyday", "my edited");
        if edited != first {
            assert_eq!(skill_to_write(Some(edited.as_bytes()), &skill), None);
        }
    }
}
