//! `thegn fleet inspect <sprite> --env <name>` — read one Sprites API object.
//!
//! The status read is an org-level GET and never executes or resumes a sprite.
//! The optional policy read is separate because the Sprites API may wake a cold
//! sprite while serving that GET.

use anyhow::{Result, anyhow, bail};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_svc::provider::Provider;
use thegn_svc::sprites::inspect::SpriteInfo;

#[derive(Debug, Clone, clap::Args)]
pub struct InspectArgs {
    /// The sprite's name as the Sprites API lists it.
    pub name: String,
    /// The sprites `[env.<name>]` whose account owns it.
    #[arg(long, value_name = "ENV")]
    pub env: String,
    /// Also read the memory policy (`GET …/policy/resources`; may wake a cold sprite).
    #[arg(long)]
    pub policy: bool,
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, serde::Serialize)]
struct InspectReport {
    name: String,
    status: String,
    created_at: Option<String>,
    url_auth: Option<String>,
    memory_limit_mb: Option<Option<u32>>,
    configured_limit_mb: Option<u32>,
    policy_read: bool,
}

pub fn run(cfg: &Config, args: InspectArgs) -> Result<()> {
    let pc = super::env_adopt::sprites_env(cfg, &args.env)?;
    let provider = crate::provider_factory::provider_for_inventory(pc)
        .map_err(|reason| anyhow!("env {}: {reason}", args.env))?;
    let Provider::Sprites(provider) = provider else {
        bail!("[env.{}] is not a sprites env", args.env);
    };
    thegn_svc::vps::registry::checked_name(&args.name)
        .map_err(|_| anyhow!("sprites: invalid sprite name"))?;
    let info = crate::agent::block_on_provider(|| async { provider.inspect(&args.name).await })?
        .ok_or_else(|| anyhow!("no sprite named {} in this account", args.name))?;
    let policy = if args.policy {
        Some(crate::agent::block_on_provider(|| async {
            provider.memory_policy_mb(&args.name).await
        })?)
    } else {
        None
    };
    let configured_mb = pc.min_ram_gb.saturating_mul(1024);
    if args.json {
        return super::emit_json(&InspectReport {
            name: info.name,
            status: info.status,
            created_at: info.created_at,
            url_auth: info.url_auth,
            memory_limit_mb: policy,
            configured_limit_mb: (configured_mb > 0).then_some(configured_mb),
            policy_read: args.policy,
        });
    }
    outln!("{}", report_lines(&info, policy, configured_mb).join("\n"));
    Ok(())
}

/// Render the human-readable report. API strings are bounded and stripped of
/// terminal controls before they are placed in output.
fn report_lines(info: &SpriteInfo, policy: Option<Option<u32>>, configured_mb: u32) -> Vec<String> {
    let name = super::fleet::sanitize_text(&info.name, 128);
    let status = super::fleet::sanitize_text(&info.status, 128);
    let created = info
        .created_at
        .as_deref()
        .map(|value| super::fleet::sanitize_text(value, 128))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".into());
    let auth = info
        .url_auth
        .as_deref()
        .map(|value| super::fleet::sanitize_text(value, 128))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "-".into());
    let mut lines = vec![
        format!("sprite {name}  status {status}  created {created}"),
        format!("url auth {auth}"),
    ];
    if auth == "public" {
        lines.push("! warning: URL auth is public".into());
    }
    if let Some(limit) = policy {
        match limit {
            Some(limit) => lines.push(format!("memory policy {limit} MiB")),
            None => lines.push("memory policy: none set".into()),
        }
        lines.push(format!(
            "configured min_ram_gb: {} ({} MiB)",
            configured_mb / 1024,
            configured_mb
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(url_auth: Option<&str>) -> SpriteInfo {
        SpriteInfo {
            name: "sx1".into(),
            status: "warm".into(),
            created_at: None,
            url_auth: url_auth.map(str::to_owned),
        }
    }

    #[test]
    fn report_lines_table() {
        let cases = [
            (
                "public warning",
                info(Some("public")),
                None,
                2048,
                vec![
                    "sprite sx1  status warm  created -",
                    "url auth public",
                    "! warning: URL auth is public",
                ],
            ),
            (
                "no policy",
                info(Some("sprite")),
                None,
                2048,
                vec!["sprite sx1  status warm  created -", "url auth sprite"],
            ),
            (
                "policy unset",
                info(None),
                Some(None),
                2048,
                vec![
                    "sprite sx1  status warm  created -",
                    "url auth -",
                    "memory policy: none set",
                    "configured min_ram_gb: 2 (2048 MiB)",
                ],
            ),
            (
                "policy set",
                info(Some("sprite")),
                Some(Some(4096)),
                2048,
                vec![
                    "sprite sx1  status warm  created -",
                    "url auth sprite",
                    "memory policy 4096 MiB",
                    "configured min_ram_gb: 2 (2048 MiB)",
                ],
            ),
        ];
        for (name, info, policy, configured_mb, expected) in cases {
            assert_eq!(
                report_lines(&info, policy, configured_mb),
                expected,
                "{name}"
            );
        }
    }

    #[test]
    fn clap_parses_policy_json_and_requires_env() {
        use clap::Parser;

        let parsed = crate::Cli::try_parse_from([
            "thegn", "fleet", "inspect", "s1", "--env", "sx", "--policy", "--json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(crate::Command::Fleet {
                action: crate::cmd::fleet::Action::Inspect(InspectArgs {
                    name,
                    env,
                    policy: true,
                    json: true,
                })
            }) if name == "s1" && env == "sx"
        ));
        assert!(crate::Cli::try_parse_from(["thegn", "fleet", "inspect", "s1"]).is_err());
    }
}
