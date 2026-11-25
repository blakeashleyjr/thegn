//! `thegn fleet inspect <sprite> --env <name>` — read one Sprites API object.
//!
//! The status read is an org-level GET and never executes or resumes a sprite.
//! The optional policy read is separate because the Sprites API may wake a cold
//! sprite while serving that GET.
//!
//! `--probe` runs one short read-only exec inside the sandbox (it wakes a cold
//! one) and reports live CPU load, memory, disk, uptime, boot id, kernel
//! OOM kills this boot, and the time left before `max_lifetime_secs` (OpenSpec
//! `remote-sandbox-visibility`, slices 1 and 2). It works for any exec-capable
//! provider env (Sprites, Vercel, VPS, Fly, machine0): a non-Sprites sandbox is
//! found in the provider's inventory by its id.

use anyhow::{Result, anyhow, bail};
use thegn_core::config::Config;
use thegn_core::outln;
use thegn_core::remote_health::{self, HealthSample};
use thegn_svc::provider::{InventoryReport, Provider};
use thegn_svc::sprites::inspect::SpriteInfo;

/// Overall deadline for the health probe exec (open + output).
pub(crate) const PROBE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

#[derive(Debug, Clone, clap::Args)]
pub struct InspectArgs {
    /// The sandbox name as the provider lists it (the sprite name for Sprites).
    pub name: String,
    /// The `[env.<name>]` whose account owns it (a sprites env, or any
    /// exec-capable provider env with `--probe`).
    #[arg(long, value_name = "ENV")]
    pub env: String,
    /// Also read the memory policy (`GET …/policy/resources`; may wake a cold sprite).
    #[arg(long)]
    pub policy: bool,
    /// Also run the health probe inside the sandbox (one exec; wakes a cold one).
    /// Works for any exec-capable provider env, not only Sprites.
    #[arg(long)]
    pub probe: bool,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    health: Option<HealthReport>,
}

/// The `--probe` result: the sample, its health level, the warnings it
/// raises, and the lifetime left; `error` when the probe could not run.
#[derive(Debug, serde::Serialize)]
pub(crate) struct HealthReport {
    level: remote_health::HealthLevel,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) sample: Option<HealthSample>,
    warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lifetime_left_secs: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// A report for a sandbox that was deliberately not probed (it is asleep and
/// a probe would wake and bill it): `unknown` with the reason.
pub(crate) fn unprobed_report(reason: String, lifetime_left_secs: Option<i64>) -> HealthReport {
    HealthReport {
        level: remote_health::HealthLevel::Unknown,
        sample: None,
        warnings: Vec::new(),
        lifetime_left_secs,
        error: Some(reason),
    }
}

/// Run the one-shot health probe in sandbox `id` through the provider's exec
/// API and turn the result into a report. `noun` names the sandbox kind in
/// warnings (`sprite`, `sandbox`). Blocking.
pub(crate) fn probe_provider(
    provider: &Provider,
    id: &str,
    lifetime_left_secs: Option<i64>,
    noun: &str,
) -> HealthReport {
    let argv = vec![
        "/bin/sh".to_string(),
        "-c".to_string(),
        remote_health::PROBE_SCRIPT.to_string(),
    ];
    let ran = crate::agent::block_on_provider(|| async {
        provider
            .run_exec_within(id, &argv, None, &[], PROBE_DEADLINE)
            .await
    });
    health_report(ran, lifetime_left_secs, noun)
}

pub fn run(cfg: &Config, args: InspectArgs) -> Result<()> {
    let pc = match cfg.env.get(&args.env) {
        Some(envc) if args.probe => &envc.provider,
        _ => super::env_adopt::sprites_env(cfg, &args.env)?,
    };
    let provider = crate::provider_factory::provider_for_inventory(pc)
        .map_err(|reason| anyhow!("env {}: {reason}", args.env))?;
    let Provider::Sprites(sprites) = &provider else {
        return run_neutral(&provider, pc, &args);
    };
    thegn_svc::vps::registry::checked_name(&args.name)
        .map_err(|_| anyhow!("sprites: invalid sprite name"))?;
    let info = crate::agent::block_on_provider(|| async { sprites.inspect(&args.name).await })?
        .ok_or_else(|| anyhow!("no sprite named {} in this account", args.name))?;
    let policy = if args.policy {
        Some(crate::agent::block_on_provider(|| async {
            sprites.memory_policy_mb(&args.name).await
        })?)
    } else {
        None
    };
    let health = args.probe.then(|| {
        let lifetime_left_secs = lifetime_left_secs(
            info.created_at.as_deref(),
            pc.max_lifetime_secs,
            thegn_core::util::now_ms(),
        );
        probe_provider(&provider, &args.name, lifetime_left_secs, "sprite")
    });
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
            health,
        });
    }
    let mut lines = report_lines(&info, policy, configured_mb);
    if let Some(health) = &health {
        lines.extend(health_lines(health));
    }
    outln!("{}", lines.join("\n"));
    Ok(())
}

/// The `--probe` path for a non-Sprites provider: the sandbox is found in the
/// provider's inventory by its id (a partial inventory still answers when the
/// row is in it), then probed.
fn run_neutral(
    provider: &Provider,
    pc: &thegn_core::config::EnvProviderConfig,
    args: &InspectArgs,
) -> Result<()> {
    if args.policy {
        bail!(
            "--policy is a Sprites-only read; use --probe for [env.{}]",
            args.env
        );
    }
    let report = crate::agent::block_on_provider(|| async { provider.inventory().await })?;
    let (rows, partial) = match report {
        InventoryReport::Complete(rows) => (rows, None),
        InventoryReport::Partial { rows, reason } => (rows, Some(reason)),
    };
    let info = rows
        .into_iter()
        .find(|row| row.id == args.name)
        .ok_or_else(|| match partial {
            Some(reason) => anyhow!(
                "no sandbox {} in the listing (the listing is partial: {reason})",
                args.name
            ),
            None => anyhow!("no sandbox named {} in env {}", args.name, args.env),
        })?;
    let lifetime = lifetime_left_from_ms(
        info.created_at_ms,
        pc.max_lifetime_secs,
        thegn_core::util::now_ms(),
    );
    let health = probe_provider(provider, &args.name, lifetime, "sandbox");
    if args.json {
        return super::emit_json(&NeutralReport {
            name: info.id,
            provider: pc.provider.trim().to_string(),
            state: info.state,
            created_at_ms: info.created_at_ms,
            health,
        });
    }
    let state = info
        .state
        .as_deref()
        .map(|s| super::fleet::sanitize_text(s, 128))
        .unwrap_or_else(|| "-".into());
    let mut lines = vec![format!(
        "sandbox {}  provider {}  state {state}",
        super::fleet::sanitize_text(&info.id, 128),
        super::fleet::sanitize_text(pc.provider.trim(), 64),
    )];
    lines.extend(health_lines(&health));
    outln!("{}", lines.join("\n"));
    Ok(())
}

/// `fleet inspect --probe --json` for a non-Sprites sandbox.
#[derive(Debug, serde::Serialize)]
struct NeutralReport {
    name: String,
    provider: String,
    state: Option<String>,
    created_at_ms: Option<i64>,
    health: HealthReport,
}

/// [`lifetime_left_secs`] from an epoch-ms creation time (inventory rows).
pub(crate) fn lifetime_left_from_ms(
    created_ms: Option<i64>,
    max_lifetime_secs: u64,
    now_ms: i64,
) -> Option<i64> {
    if max_lifetime_secs == 0 {
        return None;
    }
    let age_secs = (now_ms.saturating_sub(created_ms?)) / 1000;
    i64::try_from(max_lifetime_secs)
        .ok()
        .map(|max| max.saturating_sub(age_secs))
}

/// Seconds left before `max_lifetime_secs` from the API `created_at` (negative
/// = overdue); `None` when no lifetime is configured or the time is unreadable.
fn lifetime_left_secs(
    created_at: Option<&str>,
    max_lifetime_secs: u64,
    now_ms: i64,
) -> Option<i64> {
    if max_lifetime_secs == 0 {
        return None;
    }
    let created = chrono::DateTime::parse_from_rfc3339(created_at?.trim()).ok()?;
    let age_secs = (now_ms / 1000).saturating_sub(created.timestamp());
    i64::try_from(max_lifetime_secs)
        .ok()
        .map(|max| max.saturating_sub(age_secs))
}

/// Turn the probe exec's result into the report: a non-zero exit or an
/// unparsable output is an `error`, never a fake sample.
fn health_report(
    ran: Result<(i32, String)>,
    lifetime_left_secs: Option<i64>,
    noun: &str,
) -> HealthReport {
    let parsed = match ran {
        Ok((0, output)) => remote_health::parse_probe(&output),
        Ok((code, _)) => Err(format!("the health probe exited {code}")),
        Err(error) => Err(format!("the health probe could not run: {error:#}")),
    };
    match parsed {
        Ok(sample) => {
            let mut warnings: Vec<String> = remote_health::classify(None, &sample)
                .iter()
                .map(|event| remote_health::warning(noun, event))
                .collect();
            if let Some(left) = lifetime_left_secs
                && left < 3600
            {
                warnings.push(if left <= 0 {
                    format!("{noun} is past its max_lifetime_secs (the reaper may destroy it)")
                } else {
                    format!(
                        "{noun} lifetime expires in {}",
                        remote_health::short_duration(left.unsigned_abs())
                    )
                });
            }
            HealthReport {
                level: remote_health::level(Some(&sample)),
                sample: Some(sample),
                warnings,
                lifetime_left_secs,
                error: None,
            }
        }
        Err(error) => HealthReport {
            level: remote_health::HealthLevel::Unknown,
            sample: None,
            warnings: Vec::new(),
            lifetime_left_secs,
            error: Some(error),
        },
    }
}

/// Test helper: the report for a probe that exited 0 with `output`.
#[cfg(test)]
pub(crate) fn health_report_for_tests(output: &str) -> HealthReport {
    health_report(Ok((0, output.to_string())), None, "sandbox")
}

/// The human lines of a `--probe` report.
pub(crate) fn health_lines(health: &HealthReport) -> Vec<String> {
    let dash = || "-".to_string();
    let mut lines = Vec::new();
    if let Some(s) = &health.sample {
        let boot = s
            .boot_id
            .as_deref()
            .map(|b| super::fleet::sanitize_text(b, 64))
            .unwrap_or_else(dash);
        let uptime = s
            .uptime_secs
            .map(remote_health::short_duration)
            .unwrap_or_else(dash);
        let load = s
            .load
            .map(|[a, b, c]| format!("{a:.2} {b:.2} {c:.2}"))
            .unwrap_or_else(dash);
        let cpus = s.cpus.map(|c| c.to_string()).unwrap_or_else(dash);
        let mem = s
            .memory()
            .map(|(used, limit)| {
                format!(
                    "{} / {} ({}%)",
                    remote_health::gb(used),
                    remote_health::gb(limit),
                    s.memory_percent().unwrap_or(0)
                )
            })
            .unwrap_or_else(dash);
        let psi = s
            .mem_psi_avg10
            .map(|p| format!("  pressure {p:.1}%"))
            .unwrap_or_default();
        let disk = match (s.disk_used_kib, s.disk_total_kib) {
            (Some(used), Some(total)) => format!(
                "{} / {} ({}%)",
                remote_health::gb(used.saturating_mul(1024)),
                remote_health::gb(total.saturating_mul(1024)),
                s.disk_percent().unwrap_or(0)
            ),
            _ => dash(),
        };
        lines.push(format!(
            "health {}  uptime {uptime}  boot {boot}",
            level_word(health.level)
        ));
        lines.push(format!("cpu load {load} ({cpus} cpus)"));
        lines.push(format!("memory {mem}{psi}"));
        lines.push(format!("disk / {disk}"));
    } else {
        lines.push(format!("health {}", level_word(health.level)));
    }
    if let Some(left) = health.lifetime_left_secs {
        let text = if left <= 0 {
            "expired".to_string()
        } else {
            remote_health::short_duration(left.unsigned_abs())
        };
        lines.push(format!("lifetime left {text}"));
    }
    for warning in &health.warnings {
        lines.push(format!(
            "! warning: {}",
            super::fleet::sanitize_text(warning, 256)
        ));
    }
    if let Some(error) = &health.error {
        lines.push(format!(
            "! probe: {}",
            super::fleet::sanitize_text(error, 256)
        ));
    }
    lines
}

fn level_word(level: remote_health::HealthLevel) -> &'static str {
    match level {
        remote_health::HealthLevel::Ok => "ok",
        remote_health::HealthLevel::Warn => "warn",
        remote_health::HealthLevel::Unknown => "unknown",
    }
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
                    ..
                })
            }) if name == "s1" && env == "sx"
        ));
        assert!(crate::Cli::try_parse_from(["thegn", "fleet", "inspect", "s1"]).is_err());
        let parsed = crate::Cli::try_parse_from([
            "thegn", "fleet", "inspect", "s1", "--env", "sx", "--probe",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(crate::Command::Fleet {
                action: crate::cmd::fleet::Action::Inspect(InspectArgs { probe: true, .. })
            })
        ));
    }

    #[test]
    fn lifetime_left_table() {
        // 2026-11-25T00:00:00Z
        let created = Some("2026-11-25T00:00:00Z");
        let at = 1_795_564_800_000_i64;
        assert_eq!(lifetime_left_secs(created, 0, at), None, "no lifetime");
        assert_eq!(lifetime_left_secs(None, 3600, at), None, "no created_at");
        assert_eq!(lifetime_left_secs(Some("junk"), 3600, at), None, "bad time");
        assert_eq!(lifetime_left_secs(created, 3600, at + 600_000), Some(3000));
        assert_eq!(lifetime_left_secs(created, 60, at + 600_000), Some(-540));
    }

    #[test]
    fn lifetime_left_from_ms_table() {
        let at = 1_795_564_800_000_i64;
        assert_eq!(lifetime_left_from_ms(Some(at), 0, at), None, "no lifetime");
        assert_eq!(lifetime_left_from_ms(None, 3600, at), None, "no created");
        assert_eq!(
            lifetime_left_from_ms(Some(at), 3600, at + 600_000),
            Some(3000)
        );
        assert_eq!(
            lifetime_left_from_ms(Some(at), 60, at + 600_000),
            Some(-540)
        );
    }

    #[test]
    fn neutral_warnings_use_the_noun_and_unprobed_is_unknown() {
        let report = health_report(
            Ok((0, "thegn-health v1\nuptime 120\n".into())),
            Some(-5),
            "sandbox",
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.starts_with("sandbox is past")),
            "{:?}",
            report.warnings
        );
        let asleep = unprobed_report("not probed: sandbox is stopped".into(), None);
        assert_eq!(asleep.level, remote_health::HealthLevel::Unknown);
        assert!(asleep.sample.is_none());
    }

    #[test]
    fn clap_accepts_probe_on_any_env() {
        use clap::Parser;
        let parsed = crate::Cli::try_parse_from([
            "thegn", "fleet", "inspect", "sb1", "--env", "vx", "--probe", "--json",
        ])
        .unwrap();
        assert!(matches!(
            parsed.command,
            Some(crate::Command::Fleet {
                action: crate::cmd::fleet::Action::Inspect(InspectArgs {
                    probe: true,
                    json: true,
                    ..
                })
            })
        ));
    }

    #[test]
    fn health_report_from_a_probe_and_its_failures() {
        let probe = "thegn-health v1\nboot_id b1\nuptime 120\ncg_mem_max 4000000000\ncg_mem_current 3800000000\noom Killed process 9 (rustc) anon-rss:3612672kB\n";
        let report = health_report(Ok((0, probe.to_string())), Some(1200), "sprite");
        assert_eq!(report.level, remote_health::HealthLevel::Warn);
        assert_eq!(
            report.warnings,
            vec![
                "sprite out of memory: kernel killed rustc 3.7 GB".to_string(),
                "sprite lifetime expires in 20m".to_string(),
            ]
        );
        let lines = health_lines(&report);
        assert_eq!(lines[0], "health warn  uptime 2m  boot b1");
        assert_eq!(lines[2], "memory 3.8 GB / 4.0 GB (95%)");
        assert!(
            lines.contains(&"lifetime left 20m".to_string()),
            "{lines:?}"
        );

        let failed = health_report(Ok((127, "sh: not found".into())), None, "sprite");
        assert_eq!(failed.level, remote_health::HealthLevel::Unknown);
        assert!(failed.sample.is_none());
        assert_eq!(
            health_lines(&failed),
            vec!["health unknown", "! probe: the health probe exited 127"]
        );

        let garbage = health_report(Ok((0, "hello".into())), None, "sprite");
        assert!(garbage.error.unwrap().contains("thegn-health header"));

        let gone = health_report(
            Err(anyhow!("exec stream ended without an exit status")),
            None,
            "sandbox",
        );
        assert!(gone.error.unwrap().contains("could not run"));
    }
}
